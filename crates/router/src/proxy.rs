//! Rust-side HTTP reverse proxy (T5.4).
//!
//! The data plane matches the compiled [`RouteTable`], the [`Balancer`] picks a
//! peer, and [`proxy_request`] forwards the request over a fresh TCP connection
//! and relays the upstream response. Hop-by-hop headers are stripped both ways
//! (RFC 7230); `X-Forwarded-For`/`X-Forwarded-Proto` are added.
//!
//! **Scope B (T5.4):** when a [`rustls`] [`ServerConfig`] is supplied, the
//! listener terminates TLS (SNI selects the cert via [`crate::tls`]); plaintext
//! otherwise. A hot-reload channel ([`crate::config`]) swaps the live
//! [`RouteTable`] between requests without restarting the process — the M1 gate.
//! When no route matches and no default backend exists, an optional Lua
//! [`Pipeline`] serves the request (the T5.2 content path); otherwise 404.
//! Upstream keep-alive pooling and least-conn are T5.5 backlog.

use std::future::Future;
use std::net::SocketAddr;
use std::rc::Rc;
use std::sync::Arc;

use bytes::Bytes;
use http::request::Parts;
use http::{HeaderName, HeaderValue, Response};
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc::UnboundedReceiver;

use crate::balancer::{pick_peer, Balancer, UpstreamResolver};
use crate::config::RouteStore;
use crate::conn::{self, BodyError, UpstreamResponse};
use crate::pipeline::{build_response, Pipeline};
use crate::route::{RouteTable, UpstreamRef};

/// Forward one request to `peer` and return the upstream's response.
///
/// Sends `Connection: close`, so the entire response is read until EOF.
pub(crate) async fn proxy_request(
    peer: SocketAddr,
    parts: &Parts,
    body: &[u8],
) -> std::io::Result<Response<Bytes>> {
    let mut upstream = TcpStream::connect(peer).await?;
    let wire = build_upstream_request(parts, body);
    upstream.write_all(&wire).await?;
    upstream.flush().await?;

    let parsed = conn::read_upstream_response(&mut upstream).await?;
    Ok(to_client_response(parsed))
}


/// Build the wire bytes for an upstream HTTP/1.1 request.
fn build_upstream_request(parts: &Parts, body: &[u8]) -> Vec<u8> {
    let target = parts.uri.to_string();
    let method = parts.method.as_str();
    let mut out = Vec::with_capacity(body.len() + 512);
    out.extend_from_slice(method.as_bytes());
    out.push(b' ');
    out.extend_from_slice(target.as_bytes());
    out.extend_from_slice(b" HTTP/1.1\r\n");

    for (name, value) in parts.headers.iter() {
        let name = name.as_str();
        // Drop hop-by-hop + content-length/transfer-encoding (re-framed below).
        if conn::is_hop_by_hop(name) || name.eq_ignore_ascii_case("content-length") {
            continue;
        }
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(b": ");
        out.extend_from_slice(value.as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    if !body.is_empty() {
        out.extend_from_slice(format!("content-length: {}\r\n", body.len()).as_bytes());
    }
    out.extend_from_slice(b"connection: close\r\n\r\n");
    out.extend_from_slice(body);
    out
}

/// Turn a parsed [`UpstreamResponse`] into a client-facing [`Response`],
/// stripping hop-by-hop headers and fixing `Content-Length`.
fn to_client_response(parsed: UpstreamResponse) -> Response<Bytes> {
    let body = Bytes::from(parsed.body);
    let mut builder = Response::builder().status(
        http::StatusCode::from_u16(parsed.status)
            .unwrap_or(http::StatusCode::INTERNAL_SERVER_ERROR),
    );
    for (name, value) in &parsed.headers {
        if conn::is_hop_by_hop(name) {
            continue;
        }
        if let (Ok(n), Ok(v)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            builder = builder.header(n, v);
        }
    }
    builder = builder.header(http::header::CONTENT_LENGTH, body.len());
    builder.body(body).unwrap_or_else(|_| Response::new(Bytes::new()))
}

/// Optional, pluggable parts of the reverse-proxy data plane. `routes`,
/// `listener`, and `shutdown` stay direct parameters of [`serve_proxy`] because
/// every invocation supplies them.
pub struct ProxyOptions<R> {
    /// Round-robin peer selector shared across connections.
    pub balancer: Rc<Balancer>,
    /// Expands an `UpstreamRef` to live peer `SocketAddr`s.
    pub resolver: R,
    /// Optional Lua content pipeline (fallback when no route matches).
    pub pipeline: Option<Rc<Pipeline>>,
    /// Optional TLS termination config; SNI selects the cert when present.
    pub tls: Option<Arc<rustls::ServerConfig>>,
    /// Optional hot-reload channel yielding successive route tables.
    pub reload: Option<UnboundedReceiver<RouteTable>>,
}

/// Run the reverse-proxy data plane until `shutdown` resolves.
///
/// Must be driven on a [`tokio::task::LocalSet`]: per-connection tasks are
/// `spawn_local`-ed so the `!Send` VM (when a fallback pipeline is given) never
/// crosses a thread. TLS termination is enabled by passing a `ServerConfig`
/// (SNI selects the cert). Hot reload is enabled by passing a receiver that
/// yields successive [`RouteTable`]s — each is swapped in between requests.
pub async fn serve_proxy<R, F>(
    routes: RouteTable,
    opts: ProxyOptions<R>,
    listener: TcpListener,
    shutdown: F,
) -> std::io::Result<()>
where
    R: UpstreamResolver + 'static,
    F: Future<Output = ()>,
{
    let ProxyOptions { balancer, resolver, pipeline, tls, reload } = opts;
    if let Some(p) = &pipeline {
        p.boot().await;
    }
    let resolver = Rc::new(resolver);
    let store = RouteStore::new(routes);
    let addr = listener.local_addr()?;
    let scheme = if tls.is_some() { "HTTPS" } else { "HTTP" };
    tracing::info!(target: "init-pro", %addr, scheme, "router proxy data plane listening");

    // Hot-reload worker: each new table is swapped into the store. Safe to run
    // concurrently with serving: every request takes an `Rc` *snapshot* at
    // accept time, so a mid-flight swap never disturbs an in-flight request.
    if let Some(mut rx) = reload {
        let store = store.clone();
        tokio::task::spawn_local(async move {
            while let Some(table) = rx.recv().await {
                let gen = store.install(table);
                tracing::info!(target: "init-pro", gen, "route table reloaded");
            }
        });
    }

    let mut shutdown = Box::pin(shutdown);

    loop {
        tokio::select! {
            biased;
            _ = &mut shutdown => break,
            res = listener.accept() => {
                let (stream, peer) = match res {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!(target: "init-pro", error = %e, "accept failed");
                        continue;
                    }
                };
                let routes = store.snapshot();
                let balancer = balancer.clone();
                let resolver = resolver.clone();
                let pipeline = pipeline.clone();
                let tls = tls.clone();
                tokio::task::spawn_local(async move {
                    let proto = if tls.is_some() { "https" } else { "http" };
                    if let Some(cfg) = tls {
                        // TLS handshake; SNI selects the cert (rustls resolver).
                        match tokio_rustls::TlsAcceptor::from(cfg).accept(stream).await {
                            Ok(tls_stream) => {
                                if let Err(e) = handle_proxy_connection(
                                    tls_stream, peer, &routes, &balancer, &*resolver, pipeline.as_deref(), proto,
                                ).await {
                                    tracing::warn!(target: "init-pro", %peer, error = %e, "proxy connection closed");
                                }
                            }
                            Err(e) => tracing::warn!(target: "init-pro", %peer, error = %e, "tls handshake failed"),
                        }
                    } else {
                        if let Err(e) = handle_proxy_connection(
                            stream, peer, &routes, &balancer, &*resolver, pipeline.as_deref(), proto,
                        ).await {
                            tracing::warn!(target: "init-pro", %peer, error = %e, "proxy connection closed");
                        }
                    }
                });
            }
        }
    }
    tracing::info!(target: "init-pro", "router proxy data plane drained");
    Ok(())
}

/// Handle one proxied connection: parse, route, forward (or fall back).
///
/// Generic over the stream type so the same handler serves plaintext
/// `TcpStream` and `TlsStream<TcpStream>` (Scope B).
async fn handle_proxy_connection<S>(
    mut stream: S,
    peer: SocketAddr,
    routes: &RouteTable,
    balancer: &Balancer,
    resolver: &dyn UpstreamResolver,
    pipeline: Option<&Pipeline>,
    forwarded_proto: &str,
) -> std::io::Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let result = dispatch_request(&mut stream, peer, routes, balancer, resolver, pipeline, forwarded_proto).await;
    // Best-effort graceful close: for TLS this flushes a `close_notify` alert
    // (so the client's `read_to_end` doesn't see an abrupt EOF); for plain TCP
    // it just sends FIN. Ignored: the response is already written on success,
    // and on error the connection is dead anyway.
    let _ = stream.shutdown().await;
    result
}

/// Parse, route, forward (or fall back to the Lua pipeline). The inner half of
/// [`handle_proxy_connection`]; generic over the stream type so plaintext and
/// TLS connections share one implementation.
async fn dispatch_request<S>(
    stream: &mut S,
    peer: SocketAddr,
    routes: &RouteTable,
    balancer: &Balancer,
    resolver: &dyn UpstreamResolver,
    pipeline: Option<&Pipeline>,
    forwarded_proto: &str,
) -> std::io::Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let head = conn::read_head(stream).await?;
    let (req, body_spec) = match conn::parse_request(&head) {
        Ok(v) => v,
        Err(msg) => {
            conn::write_response(stream, conn::error_response(400, &msg)).await?;
            return Ok(());
        }
    };
    let body = match conn::read_body(stream, body_spec).await {
        Ok(b) => b,
        Err(BodyError::TooLarge) => {
            conn::write_response(stream, conn::error_response(413, "request body too large")).await?;
            return Ok(());
        }
        Err(BodyError::Io(e)) => return Err(e),
    };
    let (parts, _) = req.into_parts();

    let host = header_value(&parts.headers, "host").unwrap_or_default();
    let path = parts.uri.path();

    // Route match -> upstream -> balancer -> proxy.
    let upstream = routes
        .lookup(&host, path)
        .map(|r| &r.upstream)
        .or_else(|| routes.default_upstream());

    if let Some(up) = upstream {
        let resp = forward_upstream(up, balancer, resolver, &parts, &body, peer, forwarded_proto).await;
        conn::write_response(stream, resp).await?;
        return Ok(());
    }

    // Fallback: the Lua pipeline (content/auth), or 404.
    if let Some(p) = pipeline {
        let outcome = p.serve_request_with_body(&parts, body).await;
        conn::write_response(stream, build_response(outcome)).await?;
        return Ok(());
    }
    conn::write_response(stream, conn::error_response(404, "no matching route")).await
}

/// Resolve, balance and proxy to an upstream; degrade gracefully on failure.
async fn forward_upstream(
    upstream: &UpstreamRef,
    balancer: &Balancer,
    resolver: &dyn UpstreamResolver,
    parts: &Parts,
    body: &[u8],
    peer: SocketAddr,
    forwarded_proto: &str,
) -> Response<Bytes> {
    let Some(selected) = pick_peer(balancer, resolver, upstream) else {
        return conn::error_response(503, "no upstream peers available");
    };
    match proxy_request(selected, parts, body).await {
        Ok(resp) => with_forwarded_headers(resp, peer, forwarded_proto),
        Err(e) => {
            tracing::warn!(target: "init-pro", %selected, error = %e, "upstream proxy failed");
            conn::error_response(502, "bad gateway")
        }
    }
}

/// Add `X-Forwarded-For`/`X-Forwarded-Proto` to a proxied response.
fn with_forwarded_headers(mut resp: Response<Bytes>, peer: SocketAddr, proto: &str) -> Response<Bytes> {
    let headers = resp.headers_mut();
    if let Ok(v) = HeaderValue::from_str(&peer.to_string()) {
        headers.append("x-forwarded-for", v);
    }
    let proto_val = if proto == "https" { "https" } else { "http" };
    headers.insert("x-forwarded-proto", HeaderValue::from_static(proto_val));
    resp
}

/// Case-insensitive header lookup returning a cloned String.
fn header_value(headers: &http::HeaderMap, name: &str) -> Option<String> {
    headers.get(name).and_then(|v| v.to_str().ok()).map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::route::PortRef;

    #[test]
    fn upstream_request_strips_hop_by_hop_and_sets_content_length() {
        let req = http::Request::builder()
            .method("POST")
            .uri("/echo")
            .header("host", "api.io")
            .header("connection", "keep-alive")
            .header("transfer-encoding", "chunked")
            .header("x-custom", "yes")
            .body(())
            .unwrap();
        let (parts, _) = req.into_parts();
        let wire = String::from_utf8(build_upstream_request(&parts, b"hello")).unwrap();
        assert!(wire.starts_with("POST /echo HTTP/1.1\r\n"));
        assert!(wire.contains("host: api.io"));
        assert!(wire.contains("x-custom: yes"));
        assert!(wire.contains("content-length: 5"));
        assert!(wire.contains("connection: close\r\n\r\nhello"));
        assert!(!wire.contains("keep-alive"));
        assert!(!wire.contains("transfer-encoding: chunked"));
    }

    #[test]
    fn client_response_drops_hop_by_hop() {
        let parsed = UpstreamResponse {
            status: 200,
            headers: vec![
                ("Content-Type".into(), "text/plain".into()),
                ("Transfer-Encoding".into(), "chunked".into()),
                ("Connection".into(), "keep-alive".into()),
            ],
            body: b"hi".to_vec(),
        };
        let resp = to_client_response(parsed);
        assert_eq!(resp.status(), 200);
        assert!(resp.headers().contains_key("content-type"));
        assert!(!resp.headers().contains_key("transfer-encoding"));
        assert_eq!(resp.headers().get("content-length").unwrap(), "2");
    }

    #[test]
    fn forwarded_headers_present() {
        let resp =
            with_forwarded_headers(Response::new(Bytes::new()), "127.0.0.1:1234".parse().unwrap(), "http");
        assert_eq!(resp.headers().get("x-forwarded-proto").unwrap(), "http");
        assert_eq!(resp.headers().get("x-forwarded-for").unwrap(), "127.0.0.1:1234");
    }

    #[test]
    fn path_only_used_for_routing() {
        // sanity: uri path extraction
        let uri: http::Uri = "/a/b?x=1".parse().unwrap();
        assert_eq!(uri.path(), "/a/b");
        let _ = PortRef::Number(0); // ensure enum is in scope
    }
}
