//! Ingress gateway acceptance (workstream A, decision **Q32**): real
//! storage-backed Ingresses + Services + Endpoints drive the openrusty
//! gateway on an ephemeral `ingress_listen` port — observed by a real TCP
//! client. Host routing, the host-less/default-backend fallback, exact-path
//! scoping and the Q28 503-while-empty parity are gated here.

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

fn service_doc(name: &str, target_port: u16) -> Value {
    json!({
        "apiVersion": "v1", "kind": "Service",
        "metadata": {"name": name, "namespace": "default"},
        "spec": {
            "type": "ClusterIP",
            "ports": [{"name": "http", "port": 80, "targetPort": target_port, "protocol": "TCP"}]
        }
    })
}

fn endpoints_doc(name: &str, backend: SocketAddr) -> Value {
    json!({
        "apiVersion": "v1", "kind": "Endpoints",
        "metadata": {"name": name, "namespace": "default"},
        "subsets": [{
            "addresses": [{"ip": backend.ip().to_string()}],
            "ports": [{"name": "http", "port": backend.port(), "protocol": "TCP"}]
        }]
    })
}

/// A namespaced Ingress: one host rule with two paths + optional default.
fn ingress_doc(name: &str, host: Option<&str>, backend: &str, default: Option<&str>) -> Value {
    let mut spec = json!({
        "rules": [{
            "host": host,
            "http": {"paths": [
                {"path": "/v1", "pathType": "Prefix",
                 "backend": {"service": {"name": backend, "port": {"number": 80}}}},
                {"path": "/exact", "pathType": "Exact",
                 "backend": {"service": {"name": backend, "port": {"number": 80}}}}
            ]}
        }]
    });
    if let Some(d) = default {
        spec["defaultBackend"] = json!({"service": {"name": d, "port": {"number": 80}}});
    }
    json!({
        "apiVersion": "networking.k8s.io/v1", "kind": "Ingress",
        "metadata": {"name": name, "namespace": "default"},
        "spec": spec
    })
}

/// Raw HTTP/1.1 GET with an explicit Host; returns `(status_line, body)`.
async fn http_get(addr: SocketAddr, host: &str, path: &str) -> std::io::Result<(String, String)> {
    let mut s = TcpStream::connect(addr).await?;
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"
    );
    s.write_all(req.as_bytes()).await?;
    let mut raw = String::new();
    s.read_to_string(&mut raw).await?;
    let status = raw.lines().next().unwrap_or_default().to_string();
    let body = raw.split("\r\n\r\n").nth(1).unwrap_or_default().to_string();
    Ok((status, body))
}

/// Poll until the GET response satisfies `f` (eventual consistency window).
async fn wait_http(
    addr: SocketAddr,
    host: &str,
    path: &str,
    f: impl Fn(&(String, String)) -> bool,
) -> (String, String) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut last: Option<(String, String)> = None;
    loop {
        if let Ok(r) = http_get(addr, host, path).await {
            if f(&r) {
                return r;
            }
            last = Some(r);
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "gateway never served {host}{path} (last: {:?})",
            last.as_ref().map(|r| r.0.clone())
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

struct Gateway {
    store: Arc<dyn StorageBackend>,
    shutdown: Shutdown,
    plane: router::NodePortPlane,
    reflectors: Vec<JoinHandle<()>>,
}

/// Boot reflectors + NodePort plane with the Ingress gateway on `:0`.
async fn boot() -> Gateway {
    let store: Arc<dyn StorageBackend> = Arc::new(EmbeddedStorage::new());
    let shutdown = Shutdown::new();
    let (resolver, reflectors) = router::supervise(store.clone(), shutdown.clone());
    let cfg = router::NodePortConfig {
        addr: std::net::Ipv4Addr::LOCALHOST.into(),
        ingress_listen: Some("127.0.0.1:0".parse().unwrap()),
    };
    let plane = router::spawn_nodeport_plane(resolver, cfg, shutdown.clone());
    Gateway {
        store,
        shutdown,
        plane,
        reflectors,
    }
}

async fn teardown(p: Gateway) {
    p.shutdown.trigger();
    p.plane.drain().await;
    for h in p.reflectors {
        let _ = h.await;
    }
}

/// Wait for the gateway address (`:0` -> published via the plane handle).
async fn wait_addr(p: &Gateway) -> SocketAddr {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(addr) = p.plane.ingress_addr() {
            return addr;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "ingress gateway never bound"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

fn ing_key(name: &str) -> Key {
    Key::new("networking.k8s.io", "ingresses", "default", name)
}

/// Host routing + exact-path scoping + default backend, end to end.
#[tokio::test]
async fn gateway_routes_hosts_paths_and_default_backend() {
    let (api, api_h) = spawn_echo("api-v1").await;
    let (edge, edge_h) = spawn_echo("edge-default").await;
    let p = boot().await;

    p.store
        .create(&Key::new("", "services", "default", "api"), service_doc("api", api.port()))
        .await
        .unwrap();
    p.store
        .create(&Key::new("", "endpoints", "default", "api"), endpoints_doc("api", api))
        .await
        .unwrap();
    p.store
        .create(&Key::new("", "services", "default", "edge"), service_doc("edge", edge.port()))
        .await
        .unwrap();
    p.store
        .create(&Key::new("", "endpoints", "default", "edge"), endpoints_doc("edge", edge))
        .await
        .unwrap();
    p.store
        .create(
            &ing_key("edge"),
            ingress_doc("edge", Some("api.example.com"), "api", Some("edge")),
        )
        .await
        .unwrap();

    let addr = wait_addr(&p).await;
    // Host-exact Prefix path -> api backend.
    let r = wait_http(addr, "api.example.com", "/v1/users", |(_, b)| b == "api-v1").await;
    assert!(r.0.contains("200"));
    // Exact path also routes to the rule's backend.
    let r = wait_http(addr, "api.example.com", "/exact", |(_, b)| b == "api-v1").await;
    assert!(r.0.contains("200"));
    // Unmatched host -> default backend (host-less class).
    let r = wait_http(addr, "other.example.com", "/", |(_, b)| b == "edge-default").await;
    assert!(r.0.contains("200"));

    teardown(p).await;
    api_h.abort();
    edge_h.abort();
}

/// 503 while an Ingress backend has no Endpoints (Q28 parity body), then
/// live recovery once Endpoints appear — no gateway restart.
#[tokio::test]
async fn gateway_503_while_empty_then_recovers_live() {
    let (api, api_h) = spawn_echo("api-live").await;
    let p = boot().await;

    p.store
        .create(&Key::new("", "services", "default", "api"), service_doc("api", api.port()))
        .await
        .unwrap();
    p.store
        .create(
            &ing_key("edge"),
            ingress_doc("edge", Some("api.example.com"), "api", None),
        )
        .await
        .unwrap();

    let addr = wait_addr(&p).await;
    let (status, body) =
        wait_http(addr, "api.example.com", "/v1", |(s, _)| s.contains("503")).await;
    assert!(status.contains("503"), "status line: {status}");
    assert_eq!(body, "no upstream peers available");

    // Endpoints arrive -> the hot-applied config serves the backend.
    p.store
        .create(&Key::new("", "endpoints", "default", "api"), endpoints_doc("api", api))
        .await
        .unwrap();
    let (status, _) = wait_http(addr, "api.example.com", "/v1", |(_, b)| b == "api-live").await;
    assert!(status.contains("200"), "status line: {status}");

    teardown(p).await;
    api_h.abort();
}
