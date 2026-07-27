//! T5.4 Scope A acceptance (Q5 M1 spike): an Ingress compiles to a route table,
//! the Rust reverse proxy routes real HTTP traffic to the right upstream Service,
//! and the round-robin balancer spreads load across peers. All observed by a
//! real TCP client.
//!
//! Scope A only — plaintext HTTP. TLS termination + informer-driven hot reload
//! land in Scope B.

use std::net::SocketAddr;
use std::rc::Rc;

use router::{
    compile_ingress, ephemeral_listener, Balancer, RouteTable, StaticResolver, UpstreamRef,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::{spawn_local, JoinHandle};

use k8s_openapi::api::networking::v1::{
    HTTPIngressPath, HTTPIngressRuleValue, Ingress, IngressBackend, IngressRule,
    IngressServiceBackend, IngressSpec, ServiceBackendPort,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;

/// A trivial upstream echo server: responds `200 text/plain <body>` to every
/// request, identifying itself. Reads the request head before responding to
/// avoid a premature close racing the proxy's write.
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
            let _ = stream.flush().await;
        }
    });
    (addr, handle)
}

/// Build an Ingress routing `host` + path to a Service backend.
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

/// Send a GET over real TCP and read the full response (connection: close).
struct Reply {
    status: u16,
    body: String,
    headers: Vec<(String, String)>,
}

async fn http_get(addr: SocketAddr, path: &str, host: &str) -> Reply {
    let mut s = TcpStream::connect(addr).await.expect("connect proxy");
    let req = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    s.write_all(req.as_bytes()).await.expect("write");
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).await.expect("read");
    let text = String::from_utf8_lossy(&buf).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let headers = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(n, v)| (n.trim().to_owned(), v.trim().to_owned()))
        .collect();
    Reply {
        status,
        body: body.to_owned(),
        headers,
    }
}

fn header<'a>(hs: &'a [(String, String)], name: &str) -> Option<&'a str> {
    hs.iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

/// Start the proxy with the given routes + resolver.
fn boot(routes: RouteTable, resolver: StaticResolver) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let balancer = Rc::new(Balancer::new());
    let (addr, listener) = ephemeral_listener().expect("listener");
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let server = spawn_local(async move {
        let _ = router::serve_proxy(
            routes,
            router::ProxyOptions {
                balancer,
                resolver,
                pipeline: None,
                tls: None,
                reload: None,
            },
            listener,
            async {
                let _ = rx.await;
            },
        )
        .await;
        // keep tx alive
        drop(tx);
    });
    (addr, server)
}

/// THE acceptance gate: two hosts route to two distinct upstreams.
#[tokio::test]
async fn ingress_routes_each_host_to_its_upstream() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (a_addr, _a) = spawn_echo("hello-from-service-a").await;
            let (b_addr, _b) = spawn_echo("hello-from-service-b").await;

            let ing_a = ingress(Some("api.example.com"), "/api", "service-a", 80, "a");
            let ing_b = ingress(Some("admin.example.com"), "/", "service-b", 80, "b");
            let routes = compile_ingress(&[ing_a, ing_b]);
            assert_eq!(routes.len(), 2);

            let mut resolver = StaticResolver::new();
            resolver.set(UpstreamRef::port("service-a", 80), vec![a_addr]);
            resolver.set(UpstreamRef::port("service-b", 80), vec![b_addr]);

            let (addr, server) = boot(routes, resolver);

            // host A → service A
            let r = http_get(addr, "/api/users", "api.example.com").await;
            assert_eq!(r.status, 200);
            assert_eq!(r.body, "hello-from-service-a");
            assert_eq!(header(&r.headers, "x-forwarded-proto"), Some("http"));

            // host B → service B
            let r = http_get(addr, "/anything", "admin.example.com").await;
            assert_eq!(r.status, 200);
            assert_eq!(r.body, "hello-from-service-b");

            // unknown host → 404 (no default backend, no pipeline)
            let r = http_get(addr, "/", "stranger.example.com").await;
            assert_eq!(r.status, 404);

            // The echo servers never close; just drop the server task.
            server.abort();
        })
        .await;
}

/// Round-robin: a single upstream backed by two peers alternates between them.
#[tokio::test]
async fn round_robin_spreads_across_peers() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (p1, _) = spawn_echo("peer-1").await;
            let (p2, _) = spawn_echo("peer-2").await;

            let ing = ingress(Some("svc.example.com"), "/", "svc", 80, "rr");
            let routes = compile_ingress(&[ing]);

            let mut resolver = StaticResolver::new();
            resolver.set(UpstreamRef::port("svc", 80), vec![p1, p2]);

            let (addr, server) = boot(routes, resolver);

            let bodies: Vec<String> = vec![
                http_get(addr, "/", "svc.example.com").await.body,
                http_get(addr, "/", "svc.example.com").await.body,
                http_get(addr, "/", "svc.example.com").await.body,
                http_get(addr, "/", "svc.example.com").await.body,
            ];
            // round-robin: 1,2,1,2
            assert_eq!(bodies, vec!["peer-1", "peer-2", "peer-1", "peer-2"]);
            server.abort();
        })
        .await;
}

/// Default backend catches unmatched paths within a known host.
#[tokio::test]
async fn default_backend_serves_unmatched_requests() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (up_addr, _) = spawn_echo("default-backend").await;

            let ing = Ingress {
                metadata: ObjectMeta {
                    name: Some("d".into()),
                    namespace: Some("default".into()),
                    ..Default::default()
                },
                spec: Some(IngressSpec {
                    default_backend: Some(IngressBackend {
                        service: Some(IngressServiceBackend {
                            name: "fallback".to_owned(),
                            port: Some(ServiceBackendPort {
                                name: None,
                                number: Some(80),
                            }),
                        }),
                        resource: None,
                    }),
                    ingress_class_name: None,
                    rules: None,
                    tls: None,
                }),
                status: None,
            };
            let routes = compile_ingress(&[ing]);
            assert!(routes.default_upstream().is_some());

            let mut resolver = StaticResolver::new();
            resolver.set(UpstreamRef::port("fallback", 80), vec![up_addr]);

            let (addr, server) = boot(routes, resolver);
            let r = http_get(addr, "/whatever", "any.host.io").await;
            assert_eq!(r.status, 200);
            assert_eq!(r.body, "default-backend");
            server.abort();
        })
        .await;
}

/// No peers for a matched upstream → 503 (not a crash).
#[tokio::test]
async fn no_peers_returns_503() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let ing = ingress(Some("x.example.com"), "/", "ghost", 80, "g");
            let routes = compile_ingress(&[ing]);
            // resolver has NO entry for "ghost"
            let (addr, server) = boot(routes, StaticResolver::new());
            let r = http_get(addr, "/", "x.example.com").await;
            assert_eq!(r.status, 503);
            server.abort();
        })
        .await;
}

/// The proxy adds X-Forwarded-For carrying the client peer.
#[tokio::test]
async fn forwards_xff_header_upstream() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (up_addr, _) = spawn_echo("ok").await;
            let ing = ingress(Some("h.io"), "/", "s", 80, "xff");
            let routes = compile_ingress(&[ing]);
            let mut resolver = StaticResolver::new();
            resolver.set(UpstreamRef::port("s", 80), vec![up_addr]);
            let (addr, server) = boot(routes, resolver);
            let r = http_get(addr, "/", "h.io").await;
            // the echo server ignores headers, but the *client response* carries
            // X-Forwarded-For appended by the proxy.
            assert_eq!(r.status, 200);
            assert!(header(&r.headers, "x-forwarded-for").is_some());
            server.abort();
        })
        .await;
}
