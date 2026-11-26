//! T4.2 (Q21 parity): the JSON/watch face of the kubelet's minimal HTTP/1.1
//! client -- [`HttpJson`], one `Connection: close` exchange per call, plus
//! the long-lived chunked watch stream. Shared plumbing (error type,
//! timeouts, `tcp_connect`) lives in [`super`]; the raw registry face lives
//! in [`super::body`].

use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::framing::{find_subsequence, parse_head, parse_response, response_shape};
use crate::watch::WatchConn;

use super::{tcp_connect, HttpError, READ_TIMEOUT};

/// Plain-HTTP JSON endpoint: host + port + optional base path from the URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpJson {
    host: String,
    port: u16,
    /// Request-base path (empty unless the URL carried one); `pub(super)`
    /// for the module tests.
    pub(super) base_path: String,
    /// Static bearer token (T1.3a, Q47): `Some` stamps
    /// `Authorization: Bearer <t>` on every request/watch the kubelet
    /// loops issue — inert when the server runs auth-open (v1 default).
    token: Option<String>,
    /// Per-read timeout for one request/response exchange. Defaults to
    /// [`READ_TIMEOUT`]; the T4.5 reverse-tunnel dialer widens it past the
    /// apiserver's 25s long-poll window (Q65) via [`Self::with_read_timeout`].
    read_timeout: Duration,
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
            token: None,
            read_timeout: READ_TIMEOUT,
        })
    }

    /// Widen the per-read timeout (T4.5, Q65): the tunnel dialer's
    /// long-poll parks ~25s server-side before the first byte, so it
    /// passes ~35s here; everything else keeps the 10s default.
    pub fn with_read_timeout(mut self, timeout: Duration) -> Self {
        self.read_timeout = timeout;
        self
    }

    /// Attach the static bearer token (T1.3a, Q47, P3): kubelet-side
    /// `--token` passthrough for token-file-authenticated servers.
    pub fn with_token(mut self, token: Option<String>) -> Self {
        self.token = token;
        self
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub(super) fn target(&self, path: &str) -> String {
        format!("{}{}", self.base_path, path)
    }

    async fn connect(&self) -> Result<TcpStream, HttpError> {
        tcp_connect(&self.host, self.port).await
    }

    pub(super) fn request_head(&self, method: &str, path: &str, payload: Option<&str>) -> String {
        self.request_head_ct(method, path, payload, "application/json")
    }

    /// [`request_head`] with an explicit request Content-Type (PATCH needs
    /// `application/merge-patch+json`; T7.1 Phase B / Q37).
    pub(super) fn request_head_ct(
        &self,
        method: &str,
        path: &str,
        payload: Option<&str>,
        content_type: &str,
    ) -> String {
        let mut req = format!(
            "{method} {target} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\nAccept: application/json\r\n",
            target = self.target(path),
            host = self.host,
        );
        if let Some(t) = &self.token {
            req.push_str(&format!("Authorization: Bearer {t}\r\n"));
        }
        if let Some(p) = payload {
            req.push_str(&format!(
                "Content-Type: {content_type}\r\nContent-Length: {}\r\n",
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
        self.request_ct(method, path, body, "application/json")
            .await
    }

    /// [`request`] with an explicit request Content-Type.
    async fn request_ct(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
        content_type: &str,
    ) -> Result<(u16, Vec<u8>), HttpError> {
        let mut stream = self.connect().await?;
        let payload = body.map(|v| v.to_string());
        let req = self.request_head_ct(method, path, payload.as_deref(), content_type);
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
            let n = tokio::time::timeout(self.read_timeout, stream.read(&mut buf))
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

    /// PATCH with an RFC 7386 merge-patch body (agents carry no `/status`
    /// subresource: the runner patches `status` on the main resource, Q37).
    pub async fn patch_json(&self, path: &str, body: &Value) -> Result<(u16, Value), HttpError> {
        self.json_exchange_ct("PATCH", path, Some(body), "application/merge-patch+json")
            .await
    }

    /// PATCH with a strategic-merge body. T4.3 S51 (D7): the apiserver's
    /// `status/conditions` merge key (`type`) makes a single-condition
    /// status patch upsert in place instead of replacing the array.
    pub async fn patch_json_strategic(
        &self,
        path: &str,
        body: &Value,
    ) -> Result<(u16, Value), HttpError> {
        self.json_exchange_ct(
            "PATCH",
            path,
            Some(body),
            "application/strategic-merge-patch+json",
        )
        .await
    }

    async fn json_exchange_ct(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
        content_type: &str,
    ) -> Result<(u16, Value), HttpError> {
        let (code, bytes) = self.request_ct(method, path, body, content_type).await?;
        if bytes.is_empty() {
            return Ok((code, Value::Null));
        }
        let value = serde_json::from_slice(&bytes)
            .map_err(|e| HttpError::BadResponse(format!("invalid JSON body: {e}")))?;
        Ok((code, value))
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
