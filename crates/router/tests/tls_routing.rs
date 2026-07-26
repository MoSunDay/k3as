//! T5.4 Scope B acceptance — the **M1 "TLS host works" gate** (DoD #7, first
//! half): an Ingress compiles to a route table, the Rust reverse proxy
//! terminates TLS with SNI-based cert selection, and a real rustls client hits
//! `https://<host>` and reaches the correct upstream Service — observed over a
//! real TLS handshake on the wire.
//!
//! Certs are freshly self-signed via `rcgen` per run (R5: no committed keys).

use std::net::SocketAddr;
use std::rc::Rc;
use std::sync::Arc;

use router::{build_server_config, compile_ingress, ephemeral_listener, Balancer, CertKey, RouteTable, StaticResolver, UpstreamRef};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::{spawn_local, JoinHandle};

use k8s_openapi::api::networking::v1::{
    HTTPIngressPath, HTTPIngressRuleValue, Ingress, IngressBackend, IngressRule,
    IngressServiceBackend, IngressSpec, ServiceBackendPort,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;

/// A freshly-generated self-signed cert + key (PEM) for one SNI host.
struct TestCert {
    host: String,
    cert_pem: Vec<u8>,
    key_pem: Vec<u8>,
}

fn gen_cert(host: &str) -> TestCert {
    let params = rcgen::CertificateParams::new(vec![host.to_string()]).expect("cert params");
    let key = rcgen::KeyPair::generate().expect("keypair");
    let cert = params.self_signed(&key).expect("self-signed");
    TestCert {
        host: host.to_string(),
        cert_pem: cert.pem().into_bytes(),
        key_pem: key.serialize_pem().into_bytes(),
    }
}

/// Plaintext upstream echo: returns `200 text/plain <body>` after reading the
/// request head. Identifies itself in the body so routing is observable.
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

/// Build an Ingress routing `host` + path to a Service backend.
fn ingress(host: Option<&str>, path: &str, svc: &str, port: i32, name: &str) -> Ingress {
    Ingress {
        metadata: ObjectMeta { name: Some(name.into()), namespace: Some("default".into()), ..Default::default() },
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
                                port: Some(ServiceBackendPort { name: None, number: Some(port) }),
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

struct Reply { status: u16, body: String }

fn parse_reply(buf: &[u8]) -> Reply {
    let text = String::from_utf8_lossy(buf);
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status = head.lines().next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    Reply { status, body: body.to_owned() }
}

/// A rustls client config that trusts the supplied test certs (so the real
/// cert/SAN verification path runs — no `dangerous` shortcut).
fn client_config(trusted_pem: &[&[u8]]) -> Arc<rustls::ClientConfig> {
    let mut roots = rustls::RootCertStore::empty();
    for pem in trusted_pem {
        for cert in rustls_pemfile::certs(&mut &pem[..]).collect::<Result<Vec<_>, _>>().unwrap() {
            roots.add(cert).unwrap();
        }
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut cfg = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions().unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
    Arc::new(cfg)
}

/// HTTPS GET over a real TLS handshake; `host` is both SNI and the Host header.
async fn https_get(addr: SocketAddr, host: &str, path: &str, cfg: &Arc<rustls::ClientConfig>) -> Reply {
    let connector = tokio_rustls::TlsConnector::from(cfg.clone());
    let tcp = TcpStream::connect(addr).await.unwrap();
    let name = rustls::pki_types::ServerName::try_from(host.to_string()).unwrap();
    let mut tls = connector.connect(name, tcp).await.expect("tls handshake");
    let req = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    tls.write_all(req.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    tls.read_to_end(&mut buf).await.unwrap();
    parse_reply(&buf)
}

/// Boot the TLS proxy: builds a ServerConfig from per-host test certs.
fn boot_tls(
    routes: RouteTable,
    resolver: StaticResolver,
    certs: &[TestCert],
) -> (SocketAddr, JoinHandle<()>, Vec<Vec<u8>>) {
    let entries: Vec<(String, CertKey)> = certs
        .iter()
        .map(|c| (c.host.clone(), CertKey::pem(c.cert_pem.clone(), c.key_pem.clone())))
        .collect();
    let server_cfg = build_server_config(&entries).unwrap();
    let balancer = Rc::new(Balancer::new());
    let (addr, listener) = ephemeral_listener().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let server = spawn_local(async move {
        let _ = router::serve_proxy(routes, router::ProxyOptions { balancer, resolver, pipeline: None, tls: Some(server_cfg), reload: None }, listener, async {
            let _ = rx.await;
        }).await;
        drop(tx);
    });
    (addr, server, certs.iter().map(|c| c.cert_pem.clone()).collect())
}

/// **THE M1 GATE (TLS half):** two SNI hosts route to two distinct upstreams
/// over real TLS. `curl https://a.example.com` → service-a, and likewise for b.
#[tokio::test]
async fn tls_routes_each_host_to_its_upstream() {
    tokio::task::LocalSet::new().run_until(async {
        let (a_addr, _a) = spawn_echo("hello-from-a").await;
        let (b_addr, _b) = spawn_echo("hello-from-b").await;
        let ing_a = ingress(Some("a.example.com"), "/", "svc-a", 80, "a");
        let ing_b = ingress(Some("b.example.com"), "/", "svc-b", 80, "b");
        let routes = compile_ingress(&[ing_a, ing_b]);
        let mut resolver = StaticResolver::new();
        resolver.set(UpstreamRef::port("svc-a", 80), vec![a_addr]);
        resolver.set(UpstreamRef::port("svc-b", 80), vec![b_addr]);
        let cert_a = gen_cert("a.example.com");
        let cert_b = gen_cert("b.example.com");
        let trusted: Vec<Vec<u8>> = vec![cert_a.cert_pem.clone(), cert_b.cert_pem.clone()];
        let trusted_ref: Vec<&[u8]> = trusted.iter().map(|v| v.as_slice()).collect();
        let cfg = client_config(&trusted_ref);
        let (addr, server, _) = boot_tls(routes, resolver, &[cert_a, cert_b]);
        let r = https_get(addr, "a.example.com", "/", &cfg).await;
        assert_eq!(r.status, 200);
        assert_eq!(r.body, "hello-from-a");
        let r = https_get(addr, "b.example.com", "/", &cfg).await;
        assert_eq!(r.status, 200);
        assert_eq!(r.body, "hello-from-b");
        server.abort();
    }).await;
}

/// Unknown SNI still handshakes (default cert) and routes via the default
/// backend.
#[tokio::test]
async fn tls_unknown_host_routes_via_default_backend() {
    tokio::task::LocalSet::new().run_until(async {
        let (a_addr, _a) = spawn_echo("matched-a").await;
        let (def_addr, _d) = spawn_echo("default-backend").await;
        let ing = ingress(Some("a.example.com"), "/", "svc-a", 80, "a");
        let routes = compile_ingress(&[ing]).with_default(UpstreamRef::port("svc-default", 80));
        let mut resolver = StaticResolver::new();
        resolver.set(UpstreamRef::port("svc-a", 80), vec![a_addr]);
        resolver.set(UpstreamRef::port("svc-default", 80), vec![def_addr]);
        // Register a default cert (empty host) so unknown SNI handshakes.
        // Default cert (empty host = resolver default) but with a SAN the
        // client can verify against: connecting with that SNI yields the
        // default cert, and the (route-less) host falls to the default backend.
        let c = gen_cert("catchall.example.com");
        let cert_default = TestCert { host: String::new(), cert_pem: c.cert_pem.clone(), key_pem: c.key_pem.clone() };
        let trusted: Vec<Vec<u8>> = vec![cert_default.cert_pem.clone()];
        let trusted_ref: Vec<&[u8]> = trusted.iter().map(|v| v.as_slice()).collect();
        let cfg = client_config(&trusted_ref);
        let (addr, server, _) = boot_tls(routes, resolver, &[cert_default]);
        let r = https_get(addr, "catchall.example.com", "/", &cfg).await;
        assert_eq!(r.status, 200);
        assert_eq!(r.body, "default-backend");
        server.abort();
    }).await;
}

/// Unknown SNI with no default backend → 404 (handshake still succeeds).
#[tokio::test]
async fn tls_no_default_unknown_host_returns_404() {
    tokio::task::LocalSet::new().run_until(async {
        let ing = ingress(Some("a.example.com"), "/", "svc-a", 80, "a");
        let routes = compile_ingress(&[ing]);
        let c = gen_cert("catchall.example.com");
        let cert_default = TestCert { host: String::new(), cert_pem: c.cert_pem.clone(), key_pem: c.key_pem.clone() };
        let trusted: Vec<Vec<u8>> = vec![cert_default.cert_pem.clone()];
        let trusted_ref: Vec<&[u8]> = trusted.iter().map(|v| v.as_slice()).collect();
        let cfg = client_config(&trusted_ref);
        let (addr, server, _) = boot_tls(routes, StaticResolver::new(), &[cert_default]);
        let r = https_get(addr, "catchall.example.com", "/", &cfg).await;
        assert_eq!(r.status, 404);
        server.abort();
    }).await;
}

/// When TLS is configured, a plaintext (non-TLS) client does **not** get a
/// routed HTTP response — TLS is actually enforced on the listener.
#[tokio::test]
async fn tls_listener_rejects_plaintext() {
    tokio::task::LocalSet::new().run_until(async {
        let ing = ingress(Some("a.example.com"), "/", "svc-a", 80, "a");
        let routes = compile_ingress(&[ing]);
        let cert = gen_cert("a.example.com");
        let (addr, server, _) = boot_tls(routes, StaticResolver::new(), &[cert]);
        // Send a raw plaintext HTTP request — must not yield HTTP/1.1 200.
        let mut tcp = TcpStream::connect(addr).await.unwrap();
        tcp.write_all(b"GET / HTTP/1.1\r\nHost: a.example.com\r\n\r\n").await.unwrap();
        let mut buf = Vec::new();
        let _ = tokio::time::timeout(std::time::Duration::from_millis(500), tcp.read_to_end(&mut buf)).await;
        let text = String::from_utf8_lossy(&buf);
        assert!(!text.starts_with("HTTP/1.1 200"), "plaintext should not get a routed 200: {text}");
        server.abort();
    }).await;
}

/// ALPN negotiates `http/1.1` (the data plane is HTTP/1.1).
#[tokio::test]
async fn tls_alpn_negotiates_http11() {
    tokio::task::LocalSet::new().run_until(async {
        let ing = ingress(Some("a.example.com"), "/", "svc-a", 80, "a");
        let routes = compile_ingress(&[ing]);
        let cert = gen_cert("a.example.com");
        let trusted: Vec<Vec<u8>> = vec![cert.cert_pem.clone()];
        let trusted_ref: Vec<&[u8]> = trusted.iter().map(|v| v.as_slice()).collect();
        let cfg = client_config(&trusted_ref);
        let (addr, server, _) = boot_tls(routes, StaticResolver::new(), &[cert]);
        let connector = tokio_rustls::TlsConnector::from(cfg.clone());
        let tcp = TcpStream::connect(addr).await.unwrap();
        let name = rustls::pki_types::ServerName::try_from("a.example.com").unwrap();
        let tls = connector.connect(name, tcp).await.unwrap();
        let alpn = tls.get_ref().1.alpn_protocol();
        assert_eq!(alpn, Some(&b"http/1.1"[..]));
        server.abort();
    }).await;
}
