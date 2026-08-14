//! Minimal HTTP/1.1 client over a raw `tokio::TcpStream` (T3.1b, Q21).
//!
//! Q21 deliberately keeps the v1 kubectl transport dependency-free: the
//! apiserver serves plain HTTP/1.1 (axum http1), so one small
//! request/response exchange is all `rollout status` needs. https is
//! rejected (auth + TLS arrive with T1.3). The chunked decoder exists so a
//! future watch-stream surface (T3.4) can reuse this module unchanged.
//!
//! One connection per request (`Connection: close`): the poll loop is 4 qps
//! at most, so connection churn is irrelevant and the code stays trivial.

use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Connect timeout — a refused/unroutable server must fail fast (2s).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
/// Per-read timeout while receiving the response body (5s).
const READ_TIMEOUT: Duration = Duration::from_secs(5);
/// Hard cap on a single response (guards against a runaway server).
const MAX_RESPONSE_BYTES: usize = 32 * 1024 * 1024;

/// Bare-bones HTTP/1.1 endpoint (plain struct + functions; Q21).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HttpClient {
    pub(crate) host: String,
    pub(crate) port: u16,
}

impl HttpClient {
    /// Parse a `--server` value: `http://host:port` or bare `host:port`.
    ///
    /// `https://` is a hard error until T1.3 lands TLS (Q21); any other
    /// scheme is rejected too. A port-less host defaults to 80.
    pub(crate) fn parse(server_url: &str) -> Result<Self, String> {
        let url = server_url.trim();
        if url.is_empty() {
            return Err("server address must not be empty".to_string());
        }
        if url.starts_with("https://") {
            return Err("https not supported in v1 (Q21)".to_string());
        }
        let hostport = url.strip_prefix("http://").unwrap_or(url);
        if hostport.contains("://") {
            return Err(format!(
                "unsupported scheme in server address {server_url:?} (Q21: plain HTTP only)"
            ));
        }
        // Tolerate a single trailing '/' but reject real path components.
        let hostport = hostport.strip_suffix('/').unwrap_or(hostport);
        if hostport.contains('/') {
            return Err(format!(
                "server address {server_url:?} must not contain a path"
            ));
        }
        let (host, port) = match hostport.rsplit_once(':') {
            Some((h, p)) => (
                h.to_string(),
                p.parse::<u16>()
                    .map_err(|_| format!("invalid port in server address {server_url:?}"))?,
            ),
            None => (hostport.to_string(), 80),
        };
        if host.is_empty() {
            return Err(format!("server address {server_url:?} has an empty host"));
        }
        Ok(HttpClient { host, port })
    }

    /// GET `path`, return `(status_code, json_body)` (empty body -> Null).
    pub(crate) async fn get_json(&self, path: &str) -> Result<(u16, Value), String> {
        let addr = (self.host.as_str(), self.port);
        let connect = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(addr))
            .await
            .map_err(|_| format!("connection to {}:{} timed out", self.host, self.port))?
            .map_err(|e| format!("connection to {}:{} failed: {e}", self.host, self.port))?;
        let mut stream = connect;

        let request = format!(
            "GET {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nAccept: application/json\r\n\r\n",
            self.host
        );
        stream
            .write_all(request.as_bytes())
            .await
            .map_err(|e| format!("write request failed: {e}"))?;
        stream
            .flush()
            .await
            .map_err(|e| format!("flush failed: {e}"))?;

        // Read until EOF, or as soon as Content-Length says we are complete
        // (chunked / length-less bodies still wait for the server close).
        let mut raw: Vec<u8> = Vec::with_capacity(4096);
        let mut chunk = [0u8; 8192];
        loop {
            if let Some(needed) = complete_response_len(&raw) {
                if raw.len() >= needed {
                    break;
                }
            }
            let n = tokio::time::timeout(READ_TIMEOUT, stream.read(&mut chunk))
                .await
                .map_err(|_| "timed out reading response".to_string())?
                .map_err(|e| format!("read response failed: {e}"))?;
            if n == 0 {
                break; // EOF (we asked for Connection: close)
            }
            raw.extend_from_slice(&chunk[..n]);
            if raw.len() > MAX_RESPONSE_BYTES {
                return Err("response exceeds 32 MiB limit".to_string());
            }
        }
        parse_response(&raw)
    }
}

/// If the buffered bytes carry a determinable total length
/// (headers + Content-Length body), return it. `None` = need more bytes or
/// must wait for EOF (chunked / no length).
fn complete_response_len(raw: &[u8]) -> Option<usize> {
    let hdr_end = raw.windows(4).position(|w| w == b"\r\n\r\n")? + 4;
    let headers = std::str::from_utf8(&raw[..hdr_end]).ok()?;
    let mut content_length: Option<usize> = None;
    let mut chunked = false;
    for line in headers.split("\r\n").skip(1) {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim().to_ascii_lowercase();
        let value = value.trim();
        if name == "content-length" {
            content_length = value.parse::<usize>().ok();
        } else if name == "transfer-encoding" && value.to_ascii_lowercase().contains("chunked") {
            chunked = true;
        }
    }
    if chunked {
        return None;
    }
    content_length.map(|n| hdr_end + n)
}

/// Parse a raw HTTP/1.1 response into `(status, json)` (pure, unit-tested).
fn parse_response(raw: &[u8]) -> Result<(u16, Value), String> {
    let hdr_end = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or("malformed response: no header terminator")?;
    let headers = std::str::from_utf8(&raw[..hdr_end])
        .map_err(|_| "malformed response: non-UTF-8 headers".to_string())?;
    let mut lines = headers.split("\r\n");
    let status_line = lines.next().unwrap_or_default();
    let mut parts = status_line.split_whitespace();
    let version = parts.next().unwrap_or_default();
    if !version.starts_with("HTTP/") {
        return Err(format!("malformed status line {status_line:?}"));
    }
    let code = parts
        .next()
        .and_then(|c| c.parse::<u16>().ok())
        .ok_or_else(|| format!("malformed status line {status_line:?}"))?;

    let mut chunked = false;
    let mut content_length: Option<usize> = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim().to_ascii_lowercase();
        let value = value.trim();
        if name == "content-length" {
            content_length = value.parse::<usize>().ok();
        } else if name == "transfer-encoding" && value.to_ascii_lowercase().contains("chunked") {
            chunked = true;
        }
    }

    let body_raw = &raw[hdr_end + 4..];
    let body: Vec<u8> = if chunked {
        decode_chunked(body_raw)?
    } else if let Some(n) = content_length {
        if body_raw.len() < n {
            return Err(format!(
                "truncated response: got {} of {n} body bytes",
                body_raw.len()
            ));
        }
        body_raw[..n].to_vec()
    } else {
        body_raw.to_vec()
    };

    if body.is_empty() {
        return Ok((code, Value::Null));
    }
    let value =
        serde_json::from_slice(&body).map_err(|e| format!("invalid JSON response body: {e}"))?;
    Ok((code, value))
}

/// Decode simple chunked framing: `<hex-size>[;ext]\r\n<data>\r\n` repeated,
/// `0\r\n` then optional trailers terminated by a blank line (or EOF).
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
            return Ok(out); // trailers (if any) are ignored
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
    fn parse_accepts_http_url_and_bare_hostport() {
        let c = HttpClient::parse("http://127.0.0.1:6443").unwrap();
        assert_eq!((c.host.as_str(), c.port), ("127.0.0.1", 6443));
        let c = HttpClient::parse("127.0.0.1:8080").unwrap();
        assert_eq!((c.host.as_str(), c.port), ("127.0.0.1", 8080));
        let c = HttpClient::parse("http://localhost/").unwrap();
        assert_eq!((c.host.as_str(), c.port), ("localhost", 80));
    }

    #[test]
    fn parse_rejects_https_and_bad_addresses() {
        assert_eq!(
            HttpClient::parse("https://127.0.0.1:6443").unwrap_err(),
            "https not supported in v1 (Q21)"
        );
        assert!(HttpClient::parse("ftp://x:1").is_err());
        assert!(HttpClient::parse("").is_err());
        assert!(HttpClient::parse("http://:80").is_err());
        assert!(HttpClient::parse("http://h:notaport").is_err());
        assert!(HttpClient::parse("http://h:80/some/path").is_err());
    }

    #[test]
    fn parse_response_content_length_body() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 7\r\n\r\n{\"a\":1}";
        let (code, v) = parse_response(raw).unwrap();
        assert_eq!(code, 200);
        assert_eq!(v, serde_json::json!({"a": 1}));
    }

    #[test]
    fn parse_response_empty_body_is_null() {
        let raw = b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n";
        let (code, v) = parse_response(raw).unwrap();
        assert_eq!(code, 404);
        assert!(v.is_null());
    }

    #[test]
    fn parse_response_chunked_body() {
        // `{"a":1}` split as 5-byte + 2-byte chunks, then the 0 terminator.
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\n{\"a\":\r\n2\r\n1}\r\n0\r\n\r\n";
        let (code, v) = parse_response(raw).unwrap();
        assert_eq!(code, 200);
        assert_eq!(v, serde_json::json!({"a": 1}));
    }

    #[test]
    fn parse_response_chunked_ignores_trailers_and_extensions() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n7;ext=1\r\n{\"a\":2}\r\n0\r\nX-Trailer: y\r\n\r\n";
        let (code, v) = parse_response(raw).unwrap();
        assert_eq!(code, 200);
        assert_eq!(v, serde_json::json!({"a": 2}));
    }

    #[test]
    fn parse_response_rejects_garbage() {
        assert!(parse_response(b"not http at all").is_err());
        assert!(parse_response(b"HTTP/1.1 xyz bad\r\n\r\n").is_err());
        assert!(parse_response(b"HTTP/1.1 200 OK\r\nContent-Length: 99\r\n\r\n{\"a\":1}").is_err());
        assert!(parse_response(b"HTTP/1.1 200 OK\r\n\r\nnot json").is_err());
    }

    #[test]
    fn complete_response_len_requires_full_body() {
        let head = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\n";
        assert_eq!(complete_response_len(&head[..]), Some(head.len() + 5));
        let partial = [head.as_slice(), b"{\"a\""].concat();
        assert_eq!(complete_response_len(&partial), Some(head.len() + 5));
        // Chunked: length not determinable -> None (wait for EOF).
        let chunked = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4\r\n{\"a\"";
        assert_eq!(complete_response_len(chunked), None);
    }
}
