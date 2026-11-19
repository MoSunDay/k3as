//! Sprint 18 / **S4** (Q28) acceptance, re-platformed onto the openrusty
//! gateway (workstream A, decision **Q32**): real storage-backed Services +
//! Endpoints drive per-nodePort gateway listeners over live Endpoints —
//! observed by a real TCP client. The externally observable plane behavior
//! (200 through live backends, 503 while Endpoints are empty, live backend
//! switch, listener retirement) is unchanged.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use infra::Shutdown;
use serde_json::{json, Value};
use storage::{EmbeddedStorage, Key, StorageBackend};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

/// Trivial upstream echo server: `200 text/plain <body>` for every request.
async fn spawn_echo(body: &'static str) -> (SocketAddr, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        loop {
            let (mut stream, _) = match listener.accept().await {
                Ok(v) => v,
                Err(_) => break,
            };
            tokio::spawn(async move {
                // Consume the request head (up to the blank line).
                let mut got = Vec::with_capacity(1024);
                let mut buf = [0u8; 512];
                loop {
                    match stream.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            got.extend_from_slice(&buf[..n]);
                            if got.windows(4).any(|w| w == b"\r\n\r\n") {
                                break;
                            }
                        }
                    }
                }
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(resp.as_bytes()).await;
            });
        }
    });
    (addr, handle)
}

fn service_doc(name: &str, node_port: u16, target_port: u16) -> Value {
    json!({
        "apiVersion": "v1",
        "kind": "Service",
        "metadata": {"name": name, "namespace": "default"},
        "spec": {
            "type": "NodePort",
            "ports": [{
                "name": "http",
                "port": 80,
                "targetPort": target_port,
                "nodePort": node_port,
                "protocol": "TCP"
            }]
        }
    })
}

fn endpoints_doc(name: &str, backend: SocketAddr) -> Value {
    json!({
        "apiVersion": "v1",
        "kind": "Endpoints",
        "metadata": {"name": name, "namespace": "default"},
        "subsets": [{
            "addresses": [{"ip": backend.ip().to_string()}],
            "ports": [{"name": "http", "port": backend.port(), "protocol": "TCP"}]
        }]
    })
}

/// Raw HTTP/1.1 GET; returns `(status_line, body)`.
async fn http_get(addr: SocketAddr) -> std::io::Result<(String, String)> {
    let mut s = TcpStream::connect(addr).await?;
    s.write_all(b"GET / HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n")
        .await?;
    let mut raw = String::new();
    s.read_to_string(&mut raw).await?;
    let status = raw.lines().next().unwrap_or_default().to_string();
    let body = raw.split("\r\n\r\n").nth(1).unwrap_or_default().to_string();
    Ok((status, body))
}

/// Poll until `addr` accepts TCP (the plane bound the nodePort).
async fn wait_listen(addr: SocketAddr) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if TcpStream::connect(addr).await.is_ok() {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "nodePort never opened: {addr}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Poll until the GET response satisfies `f` (eventual consistency window).
async fn wait_http(addr: SocketAddr, f: impl Fn(&(String, String)) -> bool) -> (String, String) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut last;
    loop {
        match http_get(addr).await {
            Ok(r) => {
                if f(&r) {
                    return r;
                }
                last = format!("{} | {}", r.0, r.1);
            }
            Err(e) => last = format!("<connect/proxy error: {e}>"),
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "nodePort never served {addr}; last: {last}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Poll until `addr` refuses connections (the listener was retired).
async fn wait_closed(addr: SocketAddr) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if TcpStream::connect(addr).await.is_err() {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "nodePort never closed: {addr}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

struct Plane {
    store: Arc<dyn StorageBackend>,
    resolver: router::EndpointsResolver,
    shutdown: Shutdown,
    plane: router::NodePortPlane,
    reflectors: Vec<JoinHandle<()>>,
}

/// Boot the reflectors + plane over a fresh embedded store.
async fn boot(node_port_bind: std::net::IpAddr) -> Plane {
    let store: Arc<dyn StorageBackend> = Arc::new(EmbeddedStorage::new());
    let shutdown = Shutdown::new();
    let (resolver, reflectors) = router::supervise(store.clone(), shutdown.clone());
    let cfg = router::NodePortConfig {
        addr: node_port_bind,
        ingress_listen: None,
    };
    let plane = router::spawn_nodeport_plane(resolver.clone(), cfg, shutdown.clone());
    Plane {
        store,
        resolver,
        shutdown,
        plane,
        reflectors,
    }
}

async fn teardown(p: Plane) {
    p.shutdown.trigger();
    p.plane.drain().await;
    for h in p.reflectors {
        let _ = h.await;
    }
}

/// S4 core: a NodePort Service + live Endpoints proxy real traffic.
#[tokio::test]
async fn plane_proxies_nodeport_traffic_to_live_endpoints() {
    let (echo, echo_h) = spawn_echo("s4-plane-hit").await;
    let p = boot(std::net::Ipv4Addr::LOCALHOST.into()).await;
    let svc = Key::new("", "services", "default", "web");
    let ep = Key::new("", "endpoints", "default", "web");
    p.store
        .create(&svc, service_doc("web", 31001, echo.port()))
        .await
        .unwrap();
    p.store
        .create(&ep, endpoints_doc("web", echo))
        .await
        .unwrap();

    let addr = SocketAddr::new(std::net::Ipv4Addr::LOCALHOST.into(), 31001);
    wait_listen(addr).await;
    let (status, body) = wait_http(addr, |(_, b)| b.contains("s4-plane-hit")).await;
    assert!(status.contains("200"), "status line: {status}");
    assert_eq!(body, "s4-plane-hit");

    teardown(p).await;
    echo_h.abort();
}

/// No (or empty) Endpoints -> the proxy answers 503.
#[tokio::test]
async fn missing_endpoints_answer_503() {
    let p = boot(std::net::Ipv4Addr::LOCALHOST.into()).await;
    let svc = Key::new("", "services", "default", "ghost");
    p.store
        .create(&svc, service_doc("ghost", 31002, 8080))
        .await
        .unwrap();

    let addr = SocketAddr::new(std::net::Ipv4Addr::LOCALHOST.into(), 31002);
    wait_listen(addr).await;
    let (status, body) = wait_http(addr, |(s, _)| s.contains("503")).await;
    assert!(status.contains("503"), "status line: {status}");
    // Q28 body parity with the Lua-era proxy (gate short-circuit).
    assert_eq!(body, "no upstream peers available");

    teardown(p).await;
}

/// An Endpoints update re-resolves: traffic switches to the new backend.
#[tokio::test]
async fn endpoints_update_switches_backends_live() {
    let (a, a_h) = spawn_echo("backend-v1").await;
    let (b, b_h) = spawn_echo("backend-v2").await;
    let p = boot(std::net::Ipv4Addr::LOCALHOST.into()).await;
    let svc = Key::new("", "services", "default", "roll");
    let ep = Key::new("", "endpoints", "default", "roll");
    p.store
        .create(&svc, service_doc("roll", 31003, a.port()))
        .await
        .unwrap();
    p.store.create(&ep, endpoints_doc("roll", a)).await.unwrap();

    let addr = SocketAddr::new(std::net::Ipv4Addr::LOCALHOST.into(), 31003);
    wait_listen(addr).await;
    let (_, body) = wait_http(addr, |(_, b)| b.contains("backend-v1")).await;
    assert_eq!(body, "backend-v1");

    // Roll the Endpoints to backend B (targetPort follows the pod port).
    p.store
        .update(&svc, service_doc("roll", 31003, b.port()), None)
        .await
        .unwrap();
    p.store
        .update(&ep, endpoints_doc("roll", b), None)
        .await
        .unwrap();
    let (_, body) = wait_http(addr, |(_, b)| b.contains("backend-v2")).await;
    assert_eq!(body, "backend-v2");

    teardown(p).await;
    a_h.abort();
    b_h.abort();
}

/// T8.2 (Q57): a TrafficSplit on the entry Service switches traffic by
/// weight, weight-only edits hot-reload, and an all-zero split 503s.
#[tokio::test]
async fn traffic_split_switches_weighted_backends() {
    let (stable, stable_h) = spawn_echo("stable-v1").await;
    let (canary, canary_h) = spawn_echo("canary-v2").await;
    let p = boot(std::net::Ipv4Addr::LOCALHOST.into()).await;
    // Entry Service: NodePort, selector-less (the Router owns traffic).
    let entry = Key::new("", "services", "default", "app");
    p.store
        .create(&entry, service_doc("app", 31010, 8080))
        .await
        .unwrap();
    for (name, backend) in [("app-stable", stable), ("app-canary", canary)] {
        let svc = Key::new("", "services", "default", name);
        let ep = Key::new("", "endpoints", "default", name);
        p.store
            .create(&svc, service_doc(name, 0, backend.port()))
            .await
            .unwrap();
        // A NodePort of 0 is not allocated; strip it to keep the entry-only
        // listener set clean.
        let mut doc = service_doc(name, 0, backend.port());
        doc["spec"]["type"] = json!("ClusterIP");
        doc["spec"]["ports"][0]
            .as_object_mut()
            .unwrap()
            .remove("nodePort");
        p.store.update(&svc, doc, None).await.unwrap();
        p.store
            .create(&ep, endpoints_doc(name, backend))
            .await
            .unwrap();
    }
    let split_key = Key::new("init-pro.io", "trafficsplits", "default", "app");
    let split = |stable_w: i64, canary_w: i64| {
        json!({
            "apiVersion": "init-pro.io/v1",
            "kind": "TrafficSplit",
            "metadata": {"name": "app", "namespace": "default"},
            "spec": {"service": "app", "backends": [
                {"service": "app-stable", "weight": stable_w},
                {"service": "app-canary", "weight": canary_w}
            ]}
        })
    };
    p.store.create(&split_key, split(100, 0)).await.unwrap();

    let addr = SocketAddr::new(std::net::Ipv4Addr::LOCALHOST.into(), 31010);
    wait_listen(addr).await;
    for _ in 0..100 {
        let st = p.resolver.snapshot();
        if st.splits.contains_key("default/app") && !st.endpoints.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let st = p.resolver.snapshot();
    assert!(
        st.splits.contains_key("default/app"),
        "split not folded: {:?}",
        st.splits.keys().collect::<Vec<_>>()
    );
    assert!(!st.endpoints.is_empty(), "endpoints not folded");
    let (_, body) = wait_http(addr, |(_, b)| b.contains("stable-v1")).await;
    assert_eq!(body, "stable-v1");

    // Weight-only edit: 0/100 -> canary only.
    p.store
        .update(&split_key, split(0, 100), None)
        .await
        .unwrap();
    let (_, body) = wait_http(addr, |(_, b)| b.contains("canary-v2")).await;
    assert_eq!(body, "canary-v2");

    // All zeros: every backend disabled -> the Q28 503.
    p.store.update(&split_key, split(0, 0), None).await.unwrap();
    let (status, body) = wait_http(addr, |(s, _)| s.contains("503")).await;
    assert_eq!(body, "no upstream peers available");
    assert!(status.contains("503"), "status line: {status}");

    teardown(p).await;
    stable_h.abort();
    canary_h.abort();
}

/// Deleting the Service retires the nodePort listener.
#[tokio::test]
async fn service_delete_retires_the_listener() {
    let (echo, echo_h) = spawn_echo("retire-me").await;
    let p = boot(std::net::Ipv4Addr::LOCALHOST.into()).await;
    let svc = Key::new("", "services", "default", "bye");
    let ep = Key::new("", "endpoints", "default", "bye");
    p.store
        .create(&svc, service_doc("bye", 31004, echo.port()))
        .await
        .unwrap();
    p.store
        .create(&ep, endpoints_doc("bye", echo))
        .await
        .unwrap();

    let addr = SocketAddr::new(std::net::Ipv4Addr::LOCALHOST.into(), 31004);
    wait_listen(addr).await;
    wait_http(addr, |(_, b)| b.contains("retire-me")).await;

    p.store.delete(&svc, None).await.unwrap();
    p.store.delete(&ep, None).await.unwrap();
    wait_closed(addr).await;

    teardown(p).await;
    echo_h.abort();
}
