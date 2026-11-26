//! Minimal HTTP/1.1 exchange for the registry push client (TODO
//! **T6.3**, M1 slice; T8.1, Q38).
//!
//! One `Connection: close` TCP session per request, Content-Length
//! framing only (the V2 endpoints the client speaks always answer with
//! a length), no auth, plain HTTP. Keeping this module separate leaves
//! `client.rs` as the protocol logic and this one as the wire layer -
//! the same split `kubectl/src/http.rs` uses for the apiserver client.

use std::time::Duration;

use sha2::{Digest as _, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::client::PushError;

/// Connect timeout: a refused/unroutable registry must fail fast.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Hard cap on one response (every reply here is a status + headers).
const MAX_RESPONSE_BYTES: usize = 32 * 1024 * 1024;

/// One request target: authority (`host:port`) + absolute path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Target {
    pub(crate) authority: String,
    pub(crate) path: String,
}

/// A parsed HTTP/1.1 reply (header names lowercased).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Reply {
    pub(crate) status: u16,
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) body: Vec<u8>,
}

impl Reply {
    /// Case-insensitive header lookup; the first match wins.
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        let want = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(k, _)| *k == want)
            .map(|(_, v)| v.as_str())
    }

    /// UTF-8 lossy, trimmed body, truncated to 400 chars for error text.
    pub(crate) fn body_text(&self) -> String {
        let text = String::from_utf8_lossy(&self.body);
        let trimmed = text.trim();
        match trimmed.char_indices().nth(400) {
            Some((i, _)) => format!("{}...", &trimmed[..i]),
            None => trimmed.to_string(),
        }
    }
}

/// Resolve a `Location` header against the registry authority. The
/// server emits relative paths (`/v2/<repo>/blobs/uploads/<uuid>`).
pub(crate) fn resolve_location(authority: &str, location: &str) -> Result<Target, PushError> {
    let loc = location.trim();
    if loc.is_empty() {
        return Err(PushError::Io(
            "registry answered an empty Location".to_string(),
        ));
    }
    if loc.starts_with("https://") {
        return Err(PushError::Invalid(format!(
            "registry redirected to {loc}, but https is not supported (T8.1, Q38)"
        )));
    }
    if let Some(rest) = loc.strip_prefix("http://") {
        let (auth, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };
        if auth.is_empty() {
            return Err(PushError::Io(format!("unusable Location {location:?}")));
        }
        return Ok(Target {
            authority: auth.to_string(),
            path: path.to_string(),
        });
    }
    Ok(Target {
        authority: authority.to_string(),
        path: if loc.starts_with('/') {
            loc.to_string()
        } else {
            format!("/{loc}")
        },
    })
}

/// One HTTP/1.1 request/response on a fresh `Connection: close` socket.
pub(crate) async fn exchange(
    target: &Target,
    method: &str,
    extra: &[(&str, &str)],
    body: &[u8],
) -> Result<Reply, PushError> {
    let mut stream = connect(target).await?;
    write_head(&mut stream, target, method, extra, body.len() as u64).await?;
    if !body.is_empty() {
        stream
            .write_all(body)
            .await
            .map_err(|e| PushError::Io(format!("send request body: {e}")))?;
    }
    stream
        .flush()
        .await
        .map_err(|e| PushError::Io(format!("flush request: {e}")))?;
    read_reply(stream).await
}

/// [`exchange`] with the body streamed from a file in 256 KiB windows --
/// the A1 memory posture: a multi-GiB layer never enters RAM. The SHA-256
/// is computed WHILE copying, so the upload verifies the staged digest at
/// no extra IO and the returned digest is the one the server committed.
pub(crate) async fn exchange_file(
    target: &Target,
    method: &str,
    extra: &[(&str, &str)],
    path: &std::path::Path,
    size: u64,
) -> Result<(Reply, String), PushError> {
    let mut stream = connect(target).await?;
    write_head(&mut stream, target, method, extra, size).await?;
    let mut file = tokio::fs::File::open(path)
        .await
        .map_err(|e| PushError::Io(format!("open layer {}: {e}", path.display())))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; FILE_BUF];
    let mut copied = 0u64;
    loop {
        let n = file
            .read(&mut buf)
            .await
            .map_err(|e| PushError::Io(format!("read layer {}: {e}", path.display())))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        stream
            .write_all(&buf[..n])
            .await
            .map_err(|e| PushError::Io(format!("send layer body: {e}")))?;
        copied += n as u64;
        if copied > size {
            return Err(PushError::Io(format!(
                "layer {} grew during push (declared {size} bytes)",
                path.display()
            )));
        }
    }
    if copied != size {
        return Err(PushError::Io(format!(
            "layer {} truncated during push (declared {size}, read {copied})",
            path.display()
        )));
    }
    stream
        .flush()
        .await
        .map_err(|e| PushError::Io(format!("flush request: {e}")))?;
    let reply = read_reply(stream).await?;
    Ok((reply, format!("sha256:{:x}", hasher.finalize())))
}

/// Copy window for streamed file bodies (A1: keeps RSS flat).
const FILE_BUF: usize = 256 * 1024;

/// Connect to the target authority with a bounded connect timeout.
async fn connect(target: &Target) -> Result<TcpStream, PushError> {
    let stream = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(&target.authority))
        .await
        .map_err(|_| PushError::Io(format!("connect {}: timed out", target.authority)))?
        .map_err(|e| PushError::Io(format!("connect {}: {e}", target.authority)))?;
    let _ = stream.set_nodelay(true);
    Ok(stream)
}

/// Serialize + send the request head (Content-Length framing only).
async fn write_head(
    stream: &mut TcpStream,
    target: &Target,
    method: &str,
    extra: &[(&str, &str)],
    content_length: u64,
) -> Result<(), PushError> {
    let mut head = format!(
        "{method} {} HTTP/1.1\r\nhost: {}\r\ncontent-length: {content_length}\r\nconnection: close\r\n",
        target.path, target.authority
    );
    for (k, v) in extra {
        head.push_str(k);
        head.push_str(": ");
        head.push_str(v);
        head.push_str("\r\n");
    }
    head.push_str("\r\n");
    stream
        .write_all(head.as_bytes())
        .await
        .map_err(|e| PushError::Io(format!("send request head: {e}")))
}

/// Read the reply up to EOF or a satisfied Content-Length.
async fn read_reply(mut stream: TcpStream) -> Result<Reply, PushError> {
    let mut raw = Vec::with_capacity(1024);
    let mut chunk = [0u8; 16 * 1024];
    loop {
        let n = stream
            .read(&mut chunk)
            .await
            .map_err(|e| PushError::Io(format!("read response: {e}")))?;
        if n == 0 {
            break; // EOF (Connection: close)
        }
        raw.extend_from_slice(&chunk[..n]);
        if raw.len() > MAX_RESPONSE_BYTES {
            return Err(PushError::Io("response exceeds 32 MiB".to_string()));
        }
        if reply_complete(&raw) {
            break; // Content-Length satisfied; no need to wait for EOF
        }
    }
    parse_reply(&raw)
}

/// True once the buffered bytes hold headers + the full Content-Length
/// body. `false` (keep reading) when headers are incomplete or the reply
/// has no determinable length.
fn reply_complete(raw: &[u8]) -> bool {
    let Some(hdr_end) = raw.windows(4).position(|w| w == b"\r\n\r\n") else {
        return false;
    };
    let Ok(head) = std::str::from_utf8(&raw[..hdr_end]) else {
        return false;
    };
    for line in head.split("\r\n").skip(1) {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.trim().eq_ignore_ascii_case("content-length") {
            return match value.trim().parse::<usize>() {
                Ok(n) => raw.len() >= hdr_end + 4 + n,
                Err(_) => false,
            };
        }
    }
    false
}

/// Parse one HTTP/1.1 reply (status line + headers + Content-Length body).
fn parse_reply(raw: &[u8]) -> Result<Reply, PushError> {
    let hdr_end = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| PushError::Io("malformed response: no header terminator".to_string()))?;
    let head = std::str::from_utf8(&raw[..hdr_end])
        .map_err(|_| PushError::Io("malformed response: non-UTF-8 headers".to_string()))?;
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or_default();
    let mut parts = status_line.split_whitespace();
    if !parts.next().unwrap_or_default().starts_with("HTTP/") {
        return Err(PushError::Io(format!(
            "malformed status line {status_line:?}"
        )));
    }
    let status = parts
        .next()
        .and_then(|c| c.parse::<u16>().ok())
        .ok_or_else(|| PushError::Io(format!("malformed status line {status_line:?}")))?;

    let mut headers = Vec::new();
    let mut content_length: Option<usize> = None;
    let mut chunked = false;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim().to_ascii_lowercase();
        let value = value.trim().to_string();
        match name.as_str() {
            "content-length" => content_length = value.parse::<usize>().ok(),
            "transfer-encoding" if value.to_ascii_lowercase().contains("chunked") => chunked = true,
            _ => {}
        }
        headers.push((name, value));
    }
    if chunked {
        // The V2 endpoints this client speaks always send a
        // Content-Length; refuse rather than silently mis-frame.
        return Err(PushError::Io(
            "chunked response bodies are not supported".to_string(),
        ));
    }

    let body_raw = &raw[hdr_end + 4..];
    let body = match content_length {
        Some(n) if body_raw.len() < n => {
            return Err(PushError::Io(format!(
                "truncated response body: got {} of {n} bytes",
                body_raw.len()
            )))
        }
        Some(n) => body_raw[..n].to_vec(),
        None => body_raw.to_vec(),
    };
    Ok(Reply {
        status,
        headers,
        body,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reply_parsing_and_framing() {
        let raw = b"HTTP/1.1 202 Accepted\r\nLocation: /v2/a/blobs/uploads/u1\r\nContent-Length: 0\r\n\r\n";
        let r = parse_reply(raw).unwrap();
        assert_eq!(r.status, 202);
        assert_eq!(r.header("location"), Some("/v2/a/blobs/uploads/u1"));
        assert_eq!(r.header("LOCATION"), Some("/v2/a/blobs/uploads/u1"));
        assert!(r.body.is_empty());
        assert!(reply_complete(raw));

        let raw = b"HTTP/1.1 400 Bad Request\r\nContent-Length: 7\r\n\r\n{\"e\":1}";
        assert_eq!(parse_reply(raw).unwrap().body_text(), "{\"e\":1}");

        assert!(!reply_complete(b"HTTP/1.1 200 OK\r\nContent-Len"));
        assert!(!reply_complete(b"HTTP/1.1 200 OK\r\n\r\nabcd"));
        assert!(!reply_complete(
            b"HTTP/1.1 200 OK\r\nContent-Length: 9\r\n\r\nab"
        ));
    }

    #[test]
    fn reply_parsing_rejects_bad_framing() {
        assert!(parse_reply(b"not http").is_err());
        assert!(parse_reply(b"HTTP/1.1 200 OK\r\nContent-Length: 9\r\n\r\nab").is_err());
        assert!(
            parse_reply(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n").is_err()
        );
        assert!(parse_reply(b"HTTP/1.1 xyz Nope\r\n\r\n").is_err());
    }

    #[test]
    fn location_resolution_handles_relative_and_absolute() {
        let t = resolve_location("127.0.0.1:5000", "/v2/a/b/blobs/uploads/u1").unwrap();
        assert_eq!(t.authority, "127.0.0.1:5000");
        assert_eq!(t.path, "/v2/a/b/blobs/uploads/u1");

        let t = resolve_location("127.0.0.1:5000", "v2/a/blobs/uploads/u2").unwrap();
        assert_eq!(t.path, "/v2/a/blobs/uploads/u2");

        let t = resolve_location("127.0.0.1:5000", "http://other:6/v2/x?state=1").unwrap();
        assert_eq!(t.authority, "other:6");
        assert_eq!(t.path, "/v2/x?state=1");

        assert!(resolve_location("h:1", "https://other/v2/x").is_err());
        assert!(resolve_location("h:1", "  ").is_err());
        assert!(resolve_location("h:1", "http:///v2/x").is_err());
    }
}
