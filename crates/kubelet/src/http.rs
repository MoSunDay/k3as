//! Minimal HTTP/1.1 client for the kubelet loops (TODO **T4.2**, Q21 parity).
//!
//! Same dependency-free posture as `kubectl`'s transport: plain `http://`
//! only (https arrives with T1.3), one `Connection: close` request per call,
//! and a hand-rolled chunked decoder — required here because the apiserver
//! streams watch events as chunked `application/json`, one event per line.
//! [`WatchConn`] is generic over the read half so the incremental decoder is
//! testable over `tokio::io::duplex` without sockets (see `tests/`).

use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::watch::WatchConn;
use tokio::net::TcpStream;

use crate::framing::{find_subsequence, parse_head, parse_response, response_shape};

/// Refused/unroutable servers must fail fast.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Per-read timeout for one request/response exchange.
const READ_TIMEOUT: Duration = Duration::from_secs(10);
/// Failure modes of the minimal client (errors as values).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HttpError {
    Io(String),
    BadResponse(String),
    Connect(String),
}

impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(s) => write!(f, "HTTP I/O error: {s}"),
            Self::BadResponse(s) => write!(f, "bad HTTP response: {s}"),
            Self::Connect(s) => write!(f, "HTTP connect error: {s}"),
        }
    }
}

impl std::error::Error for HttpError {}

/// Plain-HTTP JSON endpoint: host + port + optional base path from the URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpJson {
    host: String,
    port: u16,
    base_path: String,
}

impl HttpJson {
    /// Parse `http://host:port[/base/path]`; port defaults to 80.
    /// `https://` is rejected outright (Q21: TLS lands with T1.3).
    pub fn parse_url(url: &str) -> Result<Self, String> {
        let url = url.trim();
        if url.is_empty() {
            return Err("server address must not be empty".to_string());
        }
        if url.starts_with("https://") {
            return Err("https not supported in v1 (Q21); use http://".to_string());
        }
        let rest = url.strip_prefix("http://").unwrap_or(url);
        if rest.contains("://") {
            return Err(format!("unsupported scheme in server address {url:?}"));
        }
        let (hostport, base) = match rest.find('/') {
            Some(i) => (&rest[..i], rest[i..].trim_end_matches('/')),
            None => (rest, ""),
        };
        let (host, port) = match hostport.rsplit_once(':') {
            Some((h, p)) => (
                h.to_string(),
                p.parse::<u16>()
                    .map_err(|_| format!("invalid port in {url:?}"))?,
            ),
            None => (hostport.to_string(), 80),
        };
        if host.is_empty() {
            return Err(format!("server address {url:?} has an empty host"));
        }
        Ok(Self {
            host,
            port,
            base_path: base.to_string(),
        })
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    fn target(&self, path: &str) -> String {
        format!("{}{}", self.base_path, path)
    }

    async fn connect(&self) -> Result<TcpStream, HttpError> {
        let addr = (self.host.as_str(), self.port);
        tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(addr))
            .await
            .map_err(|_| {
                HttpError::Connect(format!("connect to {}:{} timed out", self.host, self.port))
            })?
            .map_err(|e| {
                HttpError::Connect(format!(
                    "connect to {}:{} failed: {e}",
                    self.host, self.port
                ))
            })
    }

    fn request_head(&self, method: &str, path: &str, payload: Option<&str>) -> String {
        let mut req = format!(
            "{method} {target} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\nAccept: application/json\r\n",
            target = self.target(path),
            host = self.host,
        );
        if let Some(p) = payload {
            req.push_str(&format!(
                "Content-Type: application/json\r\nContent-Length: {}\r\n",
                p.len()
            ));
        }
        req.push_str("\r\n");
        if let Some(p) = payload {
            req.push_str(p);
        }
        req
    }

    /// One request/response exchange on a fresh connection. The body is
    /// decoded per Content-Length, chunked framing, or read-to-EOF.
    pub async fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
    ) -> Result<(u16, Vec<u8>), HttpError> {
        let mut stream = self.connect().await?;
        let payload = body.map(|v| v.to_string());
        let req = self.request_head(method, path, payload.as_deref());
        stream
            .write_all(req.as_bytes())
            .await
            .map_err(|e| HttpError::Io(format!("write request: {e}")))?;
        stream
            .flush()
            .await
            .map_err(|e| HttpError::Io(format!("flush request: {e}")))?;

        let mut raw: Vec<u8> = Vec::with_capacity(4096);
        let mut buf = [0u8; 8192];
        loop {
            if let Some((start, Some(len))) = response_shape(&raw) {
                if raw.len() >= start + len as usize {
                    break;
                }
            }
            let n = tokio::time::timeout(READ_TIMEOUT, stream.read(&mut buf))
                .await
                .map_err(|_| HttpError::Io("timed out reading response".to_string()))?
                .map_err(|e| HttpError::Io(format!("read response: {e}")))?;
            if n == 0 {
                break;
            }
            raw.extend_from_slice(&buf[..n]);
        }
        parse_response(&raw)
    }

    /// GET returning `(status, json)` (empty body -> `Value::Null`).
    pub async fn get_json(&self, path: &str) -> Result<(u16, Value), HttpError> {
        self.json_exchange("GET", path, None).await
    }

    /// PUT with a JSON body, returning `(status, json)`.
    pub async fn put_json(&self, path: &str, body: &Value) -> Result<(u16, Value), HttpError> {
        self.json_exchange("PUT", path, Some(body)).await
    }

    /// POST with a JSON body, returning `(status, json)`.
    pub async fn post_json(&self, path: &str, body: &Value) -> Result<(u16, Value), HttpError> {
        self.json_exchange("POST", path, Some(body)).await
    }

    /// DELETE, returning `(status, json)`.
    pub async fn delete_json(&self, path: &str) -> Result<(u16, Value), HttpError> {
        self.json_exchange("DELETE", path, None).await
    }

    async fn json_exchange(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
    ) -> Result<(u16, Value), HttpError> {
        let (code, bytes) = self.request(method, path, body).await?;
        if bytes.is_empty() {
            return Ok((code, Value::Null));
        }
        let value = serde_json::from_slice(&bytes)
            .map_err(|e| HttpError::BadResponse(format!("invalid JSON body: {e}")))?;
        Ok((code, value))
    }

    /// Open a watch stream (`GET` with the connection kept open). The caller
    /// consumes events via [`WatchConn::next_line`] until it returns `None`.
    pub async fn watch(&self, path: &str) -> Result<WatchConn<TcpStream>, HttpError> {
        let mut stream = self.connect().await?;
        let req = self.request_head("GET", path, None);
        stream
            .write_all(req.as_bytes())
            .await
            .map_err(|e| HttpError::Io(format!("write watch request: {e}")))?;
        stream
            .flush()
            .await
            .map_err(|e| HttpError::Io(format!("flush watch request: {e}")))?;

        // Read until the header/body boundary; leftover bytes are body start.
        let mut head: Vec<u8> = Vec::with_capacity(512);
        let mut buf = [0u8; 1024];
        let boundary = loop {
            if let Some(pos) = find_subsequence(&head, b"\r\n\r\n") {
                break pos + 4;
            }
            let n = tokio::time::timeout(READ_TIMEOUT, stream.read(&mut buf))
                .await
                .map_err(|_| HttpError::Io("timed out reading watch headers".to_string()))?
                .map_err(|e| HttpError::Io(format!("read watch headers: {e}")))?;
            if n == 0 {
                return Err(HttpError::BadResponse("watch closed before headers".into()));
            }
            head.extend_from_slice(&buf[..n]);
        };
        let (code, content_length, chunked) = parse_head(&head[..boundary])?;
        if code != 200 {
            return Err(HttpError::BadResponse(format!(
                "watch rejected with status {code}"
            )));
        }
        let mut conn = WatchConn::from_parts(stream, chunked, content_length);
        conn.raw.extend_from_slice(&head[boundary..]);
        conn.decode_available();
        Ok(conn)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_url_variants() {
        let c = HttpJson::parse_url("http://127.0.0.1:6443").unwrap();
        assert_eq!(
            (c.host(), c.port(), c.base_path.as_str()),
            ("127.0.0.1", 6443u16, "")
        );
        assert_eq!(c.target("/api/v1/pods"), "/api/v1/pods");
        let c = HttpJson::parse_url("example").unwrap();
        assert_eq!((c.host(), c.port()), ("example", 80u16));
        let c = HttpJson::parse_url("http://h:1/base/").unwrap();
        assert_eq!(c.target("/x"), "/base/x");
    }

    #[test]
    fn parse_url_rejects_https_and_garbage() {
        assert!(HttpJson::parse_url("https://h:1").is_err());
        assert!(HttpJson::parse_url("").is_err());
        assert!(HttpJson::parse_url("ftp://h").is_err());
        assert!(HttpJson::parse_url("http://").is_err());
        assert!(HttpJson::parse_url("http://h:notaport").is_err());
    }

    #[test]
    fn request_head_shape() {
        let c = HttpJson::parse_url("http://h:8080").unwrap();
        let head = c.request_head("PUT", "/x", Some("{\"a\":1}"));
        assert!(head.starts_with("PUT /x HTTP/1.1\r\nHost: h\r\n"));
        assert!(head.contains("Content-Length: 7\r\n"));
        assert!(head.ends_with("\r\n\r\n{\"a\":1}"));
        let get = c.request_head("GET", "/y", None);
        assert!(get.ends_with("\r\n\r\n"));
        assert!(!get.contains("Content-Length"));
    }
}
