//! Minimal HTTP/1.1 data-plane server for the Router (TODO **T5.2a**).
//!
//! Accepts on a real `TcpListener` and serves each connection as a
//! `tokio::task::spawn_local` task driving the Lua pipeline — openresty's
//! per-connection coroutine model, on the single-thread `LocalSet`. This is the
//! "real client observes wire bytes" surface the T5.2 acceptance requires.
//!
//! HTTP/1.1 parsing here is deliberately minimal: request line + headers up to
//! the blank line. It reads and discards a request body when `Content-Length`
//! is present (T5.2a exposes no body API). This keeps the data plane free of the
//! axum `Send` bound (the `!Send` worker VM cannot live behind axum's
//! multi-thread router); a richer hyper-based transport is revisited in T5.2c+.

use std::future::Future;

use bytes::Bytes;
use http::{Method, Request, Response};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use std::net::SocketAddr;
use tokio::net::{TcpListener, TcpStream};

use crate::pipeline::{build_response, Pipeline};

/// Run the Router data plane until `shutdown` resolves.
///
/// Must be driven on a [`LocalSet`] (the caller's): per-connection tasks are
/// `spawn_local`-ed so the `!Send` VM never crosses a thread.
pub async fn serve<F>(
    pipeline: Pipeline,
    listener: TcpListener,
    shutdown: F,
) -> std::io::Result<()>
where
    F: Future<Output = ()>,
{
    let addr = listener.local_addr()?;
    tracing::info!(target: "init-pro", %addr, "router data plane listening (HTTP)");
    let pipeline = std::rc::Rc::new(pipeline);
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
                let pipeline = pipeline.clone();
                tokio::task::spawn_local(async move {
                    if let Err(e) = handle_connection(stream, &pipeline).await {
                        tracing::warn!(target: "init-pro", %peer, error = %e, "connection closed");
                    }
                });
            }
        }
    }
    tracing::info!(target: "init-pro", "router data plane drained");
    Ok(())
}

async fn handle_connection(
    mut stream: TcpStream,
    pipeline: &Pipeline,
) -> std::io::Result<()> {
    let head = read_head(&mut stream).await?;
    let (req, content_length) = match parse_request(&head) {
        Ok(v) => v,
        Err(msg) => {
            write_response(&mut stream, error_response(400, &msg)).await?;
            return Ok(());
        }
    };
    // Drain & discard any request body (no Lua body API in T5.2a).
    if content_length > 0 {
        let mut left = content_length;
        let mut buf = [0u8; 4096];
        while left > 0 {
            let cap = buf.len().min(left);
            let n = stream.read(&mut buf[..cap]).await?;
            if n == 0 {
                break;
            }
            left -= n;
        }
    }

    let outcome = pipeline.serve_request(req).await;
    let response = build_response(outcome);
    write_response(&mut stream, response).await
}

/// Read the request head: bytes up to and including the blank `\r\n` line.
async fn read_head(stream: &mut TcpStream) -> std::io::Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(1024);
    let mut byte = [0u8; 1];
    loop {
        // Cap a request head at 64 KiB.
        if buf.len() > 65536 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "request head too large",
            ));
        }
        let n = stream.read(&mut byte).await?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionAborted,
                "eof before end of head",
            ));
        }
        buf.push(byte[0]);
        let len = buf.len();
        if len >= 4 && &buf[len - 4..] == b"\r\n\r\n" {
            break;
        }
    }
    Ok(buf)
}

/// Parse the head into a [`Request`] and the declared `Content-Length`.
fn parse_request(head: &[u8]) -> Result<(Request<()>, usize), String> {
    let text = std::str::from_utf8(head).map_err(|_| "non-utf8 request head")?;
    let mut lines = text.split("\r\n");
    let request_line = lines.next().ok_or("empty request")?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().ok_or("missing method")?;
    let target = parts.next().ok_or("missing request target")?;
    let _version = parts.next().ok_or("missing version")?;

    let mut req = Request::builder().method(parse_method(method)?);
    let mut content_length = 0usize;
    for line in lines {
        if line.is_empty() {
            break;
        }
        let (name, value) = line.split_once(':').ok_or("malformed header line")?;
        let name = name.trim();
        let value = value.trim();
        if name.eq_ignore_ascii_case("content-length") {
            content_length = value.parse().unwrap_or(0);
        }
        if let (Ok(n), Ok(v)) = (
            http::HeaderName::from_bytes(name.as_bytes()),
            http::HeaderValue::from_str(value),
        ) {
            req = req.header(n, v);
        }
    }
    let req = req
        .uri(target)
        .body(())
        .map_err(|e| format!("bad request: {e}"))?;
    Ok((req, content_length))
}

fn parse_method(m: &str) -> Result<Method, String> {
    Method::from_bytes(m.as_bytes()).map_err(|_| format!("bad method {m}"))
}

/// Serialise an [`http::Response`] onto the wire (HTTP/1.1, connection: close).
async fn write_response(stream: &mut TcpStream, resp: Response<Bytes>) -> std::io::Result<()> {
    let (parts, body) = resp.into_parts();
    let status = parts.status.as_u16();
    let reason = reason_phrase(status);
    let mut out = Vec::with_capacity(body.len() + 256);
    out.extend_from_slice(format!("HTTP/1.1 {status} {reason}\r\n").as_bytes());
    for (name, value) in &parts.headers {
        out.extend_from_slice(name.as_str().as_bytes());
        out.extend_from_slice(b": ");
        out.extend_from_slice(value.as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b"connection: close\r\n");
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(&body);
    stream.write_all(&out).await?;
    stream.flush().await?;
    Ok(())
}

fn error_response(status: u16, msg: &str) -> Response<Bytes> {
    let body = Bytes::from(msg.as_bytes().to_vec());
    let mut resp = Response::builder()
        .status(http::StatusCode::from_u16(status).unwrap_or(http::StatusCode::BAD_REQUEST));
    resp = resp.header(http::header::CONTENT_TYPE, "text/plain");
    resp = resp.header(http::header::CONTENT_LENGTH, body.len());
    resp.body(body).unwrap()
}

fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        301 => "Moved Permanently",
        302 => "Found",
        304 => "Not Modified",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "OK",
    }
}

/// Bind an ephemeral loopback listener (mainly for tests): returns addr + listener.
pub fn ephemeral_listener() -> std::io::Result<(SocketAddr, TcpListener)> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let addr = listener.local_addr()?;
    // Move into tokio's non-blocking domain.
    listener.set_nonblocking(true)?;
    let listener = TcpListener::from_std(listener)?;
    Ok((addr, listener))
}
