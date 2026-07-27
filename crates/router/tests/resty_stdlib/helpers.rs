//! Shared helpers for the `resty_stdlib` integration-test binary.
//!
//! Stateless modules run synchronously on a bare [`router::worker_vm`]; the
//! shared-DICT gate drives two real requests through one pipeline/VM (and a
//! real-TCP variant mirroring the T5.2 §6 gate); the `resty.http`/`resty.lock`
//! suites drive real sockets against a minimal in-process responder.
//!
//! Everything here is `pub(crate)` so the per-library submodules can call into
//! it without re-defining the helpers.

use http::Request;
use router::{build_response, worker_vm, Pipeline};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Eval a Lua chunk on a *fresh* worker VM (for stateless assertions).
pub(crate) fn eval<T: mlua::FromLuaMulti>(src: &str) -> T {
    let lua = worker_vm().expect("worker vm");
    lua.load(src).eval::<T>().expect("lua eval")
}

pub(crate) fn get(path: &str) -> Request<()> {
    Request::builder().method("GET").uri(path).body(()).unwrap()
}

pub(crate) async fn body_of(p: &Pipeline, req: Request<()>) -> String {
    String::from_utf8_lossy(build_response(p.serve_request(req).await).body()).into_owned()
}

// Minimal HTTP/1.1 client (mirrors tests/phase_chain.rs).
pub(crate) async fn http_get(
    addr: std::net::SocketAddr,
    path: &str,
) -> (u16, Vec<(String, String)>, Vec<u8>) {
    let mut s = TcpStream::connect(addr).await.expect("connect");
    s.write_all(
        format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nconnection: close\r\n\r\n").as_bytes(),
    )
    .await
    .expect("write");
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).await.expect("read");
    let text = String::from_utf8_lossy(&buf);
    let status: u16 = text
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1)?.parse().ok())
        .unwrap_or(0);
    let mut headers = Vec::new();
    let mut body_start = 0;
    for (i, line) in text.split("\r\n").enumerate() {
        if line.is_empty() {
            body_start = text
                .split("\r\n")
                .take(i)
                .map(|l| l.len() + 2)
                .sum::<usize>()
                + 2;
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_owned(), v.trim().to_owned()));
        }
    }
    (status, headers, buf[body_start.min(buf.len())..].to_vec())
}

pub(crate) struct TestCert {
    pub(crate) cert_pem: Vec<u8>,
    pub(crate) key_pem: Vec<u8>,
}

pub(crate) fn gen_test_cert(host: &str) -> TestCert {
    let params = rcgen::CertificateParams::new(vec![host.to_string()]).expect("cert params");
    let key = rcgen::KeyPair::generate().expect("keypair");
    let cert = params.self_signed(&key).expect("self-signed");
    TestCert {
        cert_pem: cert.pem().into_bytes(),
        key_pem: key.serialize_pem().into_bytes(),
    }
}

/// Minimal HTTP/1.1 responder: reads (and discards) the request, then writes a
/// fixed `200` response and closes. If `tls_cfg` is set, the accepted stream is
/// wrapped in TLS (so we can exercise `resty.http` over HTTPS).
pub(crate) async fn spawn_responder(
    tls_cfg: Option<Arc<rustls::ServerConfig>>,
    body: &'static str,
) -> (std::net::SocketAddr, tokio::sync::oneshot::Sender<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    let (tx, rx) = tokio::sync::oneshot::channel();
    tokio::task::spawn_local(async move {
        let mut rx = Box::pin(rx);
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nX-Greet: hi\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        loop {
            tokio::select! {
                biased;
                _ = &mut rx => break,
                res = listener.accept() => {
                    let (sock, _) = match res { Ok(s) => s, Err(_) => continue };
                    let acc = tls_cfg.clone().map(tokio_rustls::TlsAcceptor::from);
                    let resp = resp.clone();
                    tokio::task::spawn_local(async move {
                        let mut buf = [0u8; 1024];
                        match acc {
                            Some(a) => if let Ok(mut s) = a.accept(sock).await {
                                let _ = s.read(&mut buf).await;
                                let _ = s.write_all(resp.as_bytes()).await;
                                // Clean TLS close_notify so the client's
                                // read-to-EOF is not treated as a truncation.
                                let _ = s.shutdown().await;
                            },
                            None => {
                                let mut s = sock;
                                let _ = s.read(&mut buf).await;
                                let _ = s.write_all(resp.as_bytes()).await;
                                let _ = s.shutdown().await;
                            }
                        }
                    });
                }
            }
        }
    });
    (addr, tx)
}
