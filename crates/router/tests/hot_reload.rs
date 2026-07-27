//! T5.4 Scope B acceptance — the **M1 "no-restart hot reload" gate** (DoD #7,
//! second half): a second Ingress, pushed through the config-source channel,
//! updates routing **without restarting the process**. Plus the underlying
//! [`router::RouteStore`] generation/snapshot guarantees.

use std::net::SocketAddr;
use std::rc::Rc;

use router::{
    compile_ingress, ephemeral_listener, reload_channel, Balancer, RouteStore, RouteTable,
    StaticResolver, UpstreamRef,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::{spawn_local, JoinHandle};

use k8s_openapi::api::networking::v1::{
    HTTPIngressPath, HTTPIngressRuleValue, Ingress, IngressBackend, IngressRule,
    IngressServiceBackend, IngressSpec, ServiceBackendPort,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;

/// Plaintext upstream echo (reads head, replies with a fixed identifying body).
async fn spawn_echo(body: &'static str) -> (SocketAddr, JoinHandle<()>) {
    let std_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = std_listener.local_addr().unwrap();
    std_listener.set_nonblocking(true).unwrap();
    let listener = TcpListener::from_std(std_listener).unwrap();
    let handle = spawn_local(async move {
        loop {
            let (mut stream, _) = match listener.accept().await {
                Ok(v) => v,
                Err(_) => break,
            };
            let mut got = Vec::with_capacity(1024);
            let mut buf = [0u8; 256];
            loop {
                match stream.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => {
                        got.extend_from_slice(&buf[..n]);
                        if got.windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(resp.as_bytes()).await;
            let _ = stream.flush().await;
        }
    });
    (addr, handle)
}

fn ingress(host: Option<&str>, path: &str, svc: &str, port: i32, name: &str) -> Ingress {
    Ingress {
        metadata: ObjectMeta {
            name: Some(name.into()),
            namespace: Some("default".into()),
            ..Default::default()
        },
        spec: Some(IngressSpec {
            default_backend: None,
            ingress_class_name: None,
            rules: Some(vec![IngressRule {
                host: host.map(str::to_owned),
                http: Some(HTTPIngressRuleValue {
                    paths: vec![HTTPIngressPath {
                        backend: IngressBackend {
                            service: Some(IngressServiceBackend {
                                name: svc.to_owned(),
                                port: Some(ServiceBackendPort {
                                    name: None,
                                    number: Some(port),
                                }),
                            }),
                            resource: None,
                        },
                        path: Some(path.to_owned()),
                        path_type: "Prefix".to_owned(),
                    }],
                }),
            }]),
            tls: None,
        }),
        status: None,
    }
}

async fn http_get(addr: SocketAddr, host: &str) -> u16 {
    let mut tcp = TcpStream::connect(addr).await.unwrap();
    tcp.write_all(
        format!("GET / HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n").as_bytes(),
    )
    .await
    .unwrap();
    let mut buf = Vec::new();
    tcp.read_to_end(&mut buf).await.unwrap();
    let text = String::from_utf8_lossy(&buf);
    text.lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

async fn http_get_body(addr: SocketAddr, host: &str) -> String {
    let mut tcp = TcpStream::connect(addr).await.unwrap();
    tcp.write_all(
        format!("GET / HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n").as_bytes(),
    )
    .await
    .unwrap();
    let mut buf = Vec::new();
    tcp.read_to_end(&mut buf).await.unwrap();
    let text = String::from_utf8_lossy(&buf);
    text.split_once("\r\n\r\n")
        .map(|(_, b)| b.to_owned())
        .unwrap_or_default()
}

/// **THE M1 GATE (reload half):** a second Ingress, pushed via the config
/// channel, re-routes traffic with no process restart. Initially only
/// `a.example.com` routes; after pushing a table that also includes
/// `b.example.com`, `b` reaches its upstream — same listener, same process.
#[tokio::test]
async fn hot_reload_routes_new_ingress_without_restart() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (a_addr, _a) = spawn_echo("svc-a").await;
            let (b_addr, _b) = spawn_echo("svc-b").await;

            let ing_a = ingress(Some("a.example.com"), "/", "svc-a", 80, "a");
            let ing_b = ingress(Some("b.example.com"), "/", "svc-b", 80, "b");

            let mut resolver = StaticResolver::new();
            resolver.set(UpstreamRef::port("svc-a", 80), vec![a_addr]);
            resolver.set(UpstreamRef::port("svc-b", 80), vec![b_addr]);

            let routes_a = compile_ingress(std::slice::from_ref(&ing_a));
            let (tx, rx) = reload_channel();
            let balancer = Rc::new(Balancer::new());
            let (addr, listener) = ephemeral_listener().unwrap();
            let (stx, srx) = tokio::sync::oneshot::channel::<()>();
            let server = spawn_local(async move {
                let _ = router::serve_proxy(
                    routes_a,
                    router::ProxyOptions {
                        balancer,
                        resolver,
                        pipeline: None,
                        tls: None,
                        reload: Some(rx),
                    },
                    listener,
                    async {
                        let _ = srx.await;
                    },
                )
                .await;
                drop(stx);
            });

            // Baseline: a routes, b does not.
            assert_eq!(http_get_body(addr, "a.example.com").await, "svc-a");
            assert_eq!(http_get(addr, "b.example.com").await, 404);

            // Push a second Ingress via the hot-reload channel (no restart).
            let routes_ab = compile_ingress(&[ing_a, ing_b]);
            tx.send(routes_ab).unwrap();
            // Let the single-thread reload task drain the channel.
            for _ in 0..20 {
                tokio::task::yield_now().await;
            }

            // Now b routes to its upstream, on the same process/listener.
            assert_eq!(http_get_body(addr, "b.example.com").await, "svc-b");
            // a still works (the new table is a full superset).
            assert_eq!(http_get_body(addr, "a.example.com").await, "svc-a");
            server.abort();
        })
        .await;
}

/// [`RouteStore`] stamps a monotonic generation on each install.
#[test]
fn route_store_generation_increments() {
    let t0 = RouteTable::new();
    let store = RouteStore::new(t0);
    assert_eq!(store.generation(), 0);
    let g1 = store.install(RouteTable::new());
    assert_eq!(g1, 1);
    assert_eq!(store.generation(), 1);
    let g2 = store.install(RouteTable::new());
    assert_eq!(g2, 2);
}

/// A snapshot taken before an install is unaffected by the swap — the
/// per-request table pinned at accept time never mutates under a reload.
#[test]
fn route_store_snapshot_is_stable_across_swap() {
    let routes_a = compile_ingress(&[ingress_fixture("a.example.com", "svc-a")]);
    let store = RouteStore::new(routes_a);
    let snap_before = store.snapshot();

    // Swap in a table that only knows b.
    let routes_b = compile_ingress(&[ingress_fixture("b.example.com", "svc-b")]);
    store.install(routes_b);

    // The pinned snapshot still resolves a (the old table).
    assert!(snap_before.lookup("a.example.com", "/").is_some());
    assert!(snap_before.lookup("b.example.com", "/").is_none());
    // The live store now resolves b, not a.
    let snap_after = store.snapshot();
    assert!(snap_after.lookup("b.example.com", "/").is_some());
    assert!(snap_after.lookup("a.example.com", "/").is_none());
}

/// Minimal single-rule Ingress fixture for the unit tests above.
fn ingress_fixture(host: &str, svc: &str) -> Ingress {
    ingress(Some(host), "/", svc, 80, svc)
}
