//! Minimal HTTP/1.1 client for the scheduler extender seam (T3.2, Q23).
//!
//! Same dependency-free pattern as the kubectl transport (Q21): one
//! `POST {path}` + JSON request/response exchange per call, `Connection:
//! close`, chunked decoding, hard size cap. Extendrs are cluster-local
//! helpers (typically localhost), so no TLS and no pooling (**Q10**: the
//! extender wire format is JSON over HTTP).

use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Connect timeout: an unreachable extender must fail fast.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
/// Per-read timeout while receiving.
const READ_TIMEOUT: Duration = Duration::from_secs(5);
/// Hard cap on a single response.
const MAX_RESPONSE_BYTES: usize = 32 * 1024 * 1024;

/// A parsed `http://host[:port][/prefix]` endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HttpClient {
    pub host: String,
    pub port: u16,
    /// URL path prefix from the configured extender URL.
    pub prefix: String,
}

impl HttpClient {
    /// Parse an extender base URL. `https://` is rejected in v1 (no TLS on
    /// the in-cluster seam); a port-less host defaults to 80.
    pub(crate) fn parse(url: &str) -> Result<Self, String> {
        let u = url.trim();
        if u.is_empty() {
            return Err("extender URL must not be empty".into());
        }
        if u.starts_with("https://") {
            return Err("https extenders are not supported in v1 (Q23)".into());
        }
        let rest = u.strip_prefix("http://").unwrap_or(u);
        if rest.contains("://") {
            return Err(format!("unsupported scheme in extender URL {url:?}"));
        }
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], rest[i..].to_string()),
            None => (rest, String::new()),
        };
        let (host, port) = match authority.rsplit_once(':') {
            Some((h, p)) => (
                h.to_string(),
                p.parse::<u16>()
                    .map_err(|_| format!("invalid port in extender URL {url:?}"))?,
            ),
            None => (authority.to_string(), 80),
        };
        if host.is_empty() {
            return Err(format!("extender URL {url:?} has an empty host"));
        }
        Ok(HttpClient {
            host,
            port,
            prefix: path.trim_end_matches('/').to_string(),
        })
    }

    /// POST `prefix + path` with a JSON body; returns `(status, json)` — an
    /// empty body parses to `Value::Null`.
    pub(crate) async fn post_json(&self, path: &str, body: &Value) -> Result<(u16, Value), String> {
        let uri = format!("{}{}", self.prefix, path);
        let addr = (self.host.as_str(), self.port);
        let connect = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(addr))
            .await
            .map_err(|_| format!("connection to {}:{} timed out", self.host, self.port))?
            .map_err(|e| format!("connection to {}:{} failed: {e}", self.host, self.port))?;
        let mut stream = connect;

        let payload = body.to_string();
        let request = format!(
            "POST {uri} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\nAccept: application/json\r\n\r\n{payload}",
            self.host,
            payload.len()
        );
        stream
            .write_all(request.as_bytes())
            .await
            .map_err(|e| format!("write request failed: {e}"))?;
        stream
            .flush()
            .await
            .map_err(|e| format!("flush failed: {e}"))?;

        let mut raw: Vec<u8> = Vec::with_capacity(4096);
        let mut chunk = [0u8; 8192];
        loop {
            if let Some(needed) = complete_response_len(&raw) {
                if raw.len() >= needed {
                    break;
                }
            }
            if raw.len() > MAX_RESPONSE_BYTES {
                return Err("extender response exceeds size cap".into());
            }
            let n = tokio::time::timeout(READ_TIMEOUT, stream.read(&mut chunk))
                .await
                .map_err(|_| "timed out reading extender response".to_string())?
                .map_err(|e| format!("read extender response failed: {e}"))?;
            if n == 0 {
                break; // EOF
            }
            raw.extend_from_slice(&chunk[..n]);
        }
        parse_response(&raw)
    }
}

/// Total bytes of a complete response (header + body), if determinable.
fn complete_response_len(raw: &[u8]) -> Option<usize> {
    let header_end = raw.windows(4).position(|w| w == b"\r\n\r\n")? + 4;
    let head = std::str::from_utf8(&raw[..header_end]).ok()?;
    let content_length = head.lines().find_map(|l| {
        let lower = l.to_ascii_lowercase();
        lower
            .strip_prefix("content-length:")
            .and_then(|v| v.trim().parse::<usize>().ok())
    });
    // chunked / length-less: caller waits for EOF
    content_length.map(|n| header_end + n)
}

/// Parse a full raw response into `(status, json_body)`.
fn parse_response(raw: &[u8]) -> Result<(u16, Value), String> {
    let header_end = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or("malformed response: no header terminator")?
        + 4;
    let head = std::str::from_utf8(&raw[..header_end]).map_err(|_| "non-UTF-8 response head")?;
    let status_line = head.lines().next().unwrap_or_default();
    let code: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .ok_or_else(|| format!("malformed status line {status_line:?}"))?;
    let lower = head.to_ascii_lowercase();
    let body_raw = &raw[header_end..];
    let body = if lower.contains("transfer-encoding: chunked") {
        decode_chunked(body_raw)?
    } else {
        body_raw.to_vec()
    };
    if body.is_empty() {
        return Ok((code, Value::Null));
    }
    let value =
        serde_json::from_slice(&body).map_err(|e| format!("invalid JSON extender body: {e}"))?;
    Ok((code, value))
}

/// Decode simple chunked framing (trailers ignored).
fn decode_chunked(body: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    loop {
        let line_end = body
            .windows(2)
            .skip(pos)
            .position(|w| w == b"\r\n")
            .map(|p| p + pos)
            .ok_or("malformed chunked body: missing chunk-size line")?;
        let size_token = std::str::from_utf8(&body[pos..line_end])
            .map_err(|_| "malformed chunked body: non-UTF-8 size".to_string())?;
        let size_hex = size_token.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_hex, 16)
            .map_err(|_| format!("malformed chunked body: bad chunk size {size_token:?}"))?;
        pos = line_end + 2;
        if size == 0 {
            return Ok(out);
        }
        let end = pos
            .checked_add(size)
            .filter(|e| *e <= body.len())
            .ok_or("malformed chunked body: truncated chunk data")?;
        out.extend_from_slice(&body[pos..end]);
        pos = end;
        if body.len() >= pos + 2 && &body[pos..pos + 2] == b"\r\n" {
            pos += 2;
        } else if pos != body.len() {
            return Err("malformed chunked body: missing CRLF after chunk".to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_accepts_url_with_path_prefix_and_port() {
        let c = HttpClient::parse("http://127.0.0.1:8889/scheduler/extender1").unwrap();
        assert_eq!(c.host, "127.0.0.1");
        assert_eq!(c.port, 8889);
        assert_eq!(c.prefix, "/scheduler/extender1");
        let bare = HttpClient::parse("http://localhost").unwrap();
        assert_eq!(bare.port, 80);
        assert_eq!(bare.prefix, "");
        assert!(HttpClient::parse("https://x").is_err());
        assert!(HttpClient::parse("http://").is_err());
    }

    #[test]
    fn parse_response_handles_content_length_chunked_and_empty() {
        let fixed = b"HTTP/1.1 200 OK\r\nContent-Length: 7\r\n\r\n{\"a\":1}";
        let (code, v) = parse_response(fixed).unwrap();
        assert_eq!((code, v), (200, serde_json::json!({"a": 1})));

        let chunked = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\n{\"a\":\r\n2\r\n1}\r\n0\r\n\r\n";
        let (code, v) = parse_response(chunked).unwrap();
        assert_eq!((code, v), (200, serde_json::json!({"a": 1})));

        let empty = b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n";
        let (code, v) = parse_response(empty).unwrap();
        assert_eq!(code, 404);
        assert!(v.is_null());

        assert!(parse_response(b"not http").is_err());
    }
}
