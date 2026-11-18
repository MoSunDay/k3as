//! Minimal HTTP/1.1 client for the kubelet loops (TODO **T4.2**, Q21 parity).
//!
//! Same dependency-free posture as `kubectl`'s transport: plain `http://`
//! only (https arrives with T1.3), one `Connection: close` request per call,
//! and a hand-rolled chunked decoder — required here because the apiserver
//! streams watch events as chunked `application/json`, one event per line.
//! [`WatchConn`] is generic over the read half so the incremental decoder is
//! testable over `tokio::io::duplex` without sockets (see `tests/`).
//!
//! Since T4.13 (Q54) the module also carries the raw-GET face the image
//! puller speaks: [`get_raw`] returns status + registry headers with the
//! entity streaming lazily off the owned connection ([`RawBody`]), so blobs
//! never buffer whole in memory.

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
        tcp_connect(&self.host, self.port).await
    }

    fn request_head(&self, method: &str, path: &str, payload: Option<&str>) -> String {
        self.request_head_ct(method, path, payload, "application/json")
    }

    /// [`request_head`] with an explicit request Content-Type (PATCH needs
    /// `application/merge-patch+json`; T7.1 Phase B / Q37).
    fn request_head_ct(
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

    /// PATCH with an RFC 7386 merge-patch body (agents carry no `/status`
    /// subresource: the runner patches `status` on the main resource, Q37).
    pub async fn patch_json(&self, path: &str, body: &Value) -> Result<(u16, Value), HttpError> {
        self.json_exchange_ct("PATCH", path, Some(body), "application/merge-patch+json")
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

// ================= raw GET face (T4.13 image pull, Q54) ====================

/// Connect with the shared fast-fail budget (refused/unroutable servers).
async fn tcp_connect(host: &str, port: u16) -> Result<TcpStream, HttpError> {
    let addr = (host, port);
    tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(addr))
        .await
        .map_err(|_| HttpError::Connect(format!("connect to {host}:{port} timed out")))?
        .map_err(|e| HttpError::Connect(format!("connect to {host}:{port} failed: {e}")))
}

/// Parsed raw response head fields (see [`parse_raw_head`]).
struct RawHead {
    status: u16,
    content_length: Option<u64>,
    chunked: bool,
    content_type: Option<String>,
    docker_content_digest: Option<String>,
}

/// Parse a raw response head: `(status, content_length, chunked,
/// content_type, docker_content_digest)`. Header names are case-insensitive.
fn parse_raw_head(head: &[u8]) -> Result<RawHead, HttpError> {
    let text = String::from_utf8_lossy(head);
    let mut lines = text.lines();
    let status = lines
        .next()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| HttpError::BadResponse("empty response head".into()))?;
    let mut parts = status.split_whitespace();
    let version = parts.next().unwrap_or("");
    let code: u16 = parts
        .next()
        .and_then(|c| c.parse().ok())
        .ok_or_else(|| HttpError::BadResponse(format!("bad status line {status:?}")))?;
    if !version.starts_with("HTTP/") {
        return Err(HttpError::BadResponse(format!(
            "bad HTTP version {version:?}"
        )));
    }
    let (mut content_length, mut chunked) = (None, false);
    let (mut content_type, mut digest) = (None, None);
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match name.trim().to_ascii_lowercase().as_str() {
            "content-length" => content_length = value.parse::<u64>().ok(),
            "transfer-encoding" => chunked = value.to_ascii_lowercase().contains("chunked"),
            "content-type" => content_type = Some(value.to_string()),
            "docker-content-digest" => digest = Some(value.to_string()),
            _ => {}
        }
    }
    Ok(RawHead {
        status: code,
        content_length,
        chunked,
        content_type,
        docker_content_digest: digest,
    })
}

/// Entity framing of a raw response body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RawFraming {
    Length,
    Chunked,
    UntilEof,
}

/// Incremental chunked-decode state — the same machine as `WatchConn`'s
/// decoder (watch.rs), kept private here because it yields raw payload
/// bytes, not lines.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ChunkState {
    Size,
    Data(u64),
    DataCrlf,
    Done,
}

/// Streaming entity of a [`get_raw`] response: `Content-Length` or chunked
/// framing decoded incrementally off the owned connection. Bytes surface
/// through [`tokio::io::AsyncRead`] in wire-sized pieces — the whole blob is
/// NEVER buffered (only [`RawBody::read_to_end`] may buffer, for small
/// bodies such as manifests). Generic over the reader so tests drive it over
/// `tokio::io::duplex` without sockets.
pub struct RawBody<S = TcpStream> {
    stream: S,
    framing: RawFraming,
    remaining: u64,
    /// Wire bytes read but not yet decoded.
    raw: Vec<u8>,
    /// Decoded payload bytes ready to hand out.
    decoded: Vec<u8>,
    chunk: ChunkState,
    eof: bool,
}

impl<S: tokio::io::AsyncRead + Unpin> RawBody<S> {
    /// Assemble from a parsed head: framing flags plus any body bytes that
    /// arrived with the head (the leftover past `\r\n\r\n`).
    pub(crate) fn from_parts(
        stream: S,
        chunked: bool,
        content_length: Option<u64>,
        leftover: Vec<u8>,
    ) -> Self {
        let (framing, remaining) = match (chunked, content_length) {
            (true, _) => (RawFraming::Chunked, 0),
            (false, Some(n)) => (RawFraming::Length, n),
            (false, None) => (RawFraming::UntilEof, 0),
        };
        Self {
            stream,
            framing,
            remaining,
            raw: leftover,
            decoded: Vec::new(),
            chunk: ChunkState::Size,
            eof: false,
        }
    }

    /// Convenience for SMALL bodies (manifests, configs): buffers the whole
    /// entity. Blobs must stream through [`AsyncRead`] instead.
    pub async fn read_to_end(&mut self) -> std::io::Result<Vec<u8>> {
        let mut out = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(self, &mut out).await?;
        Ok(out)
    }

    /// Terminal condition per framing: length drained, terminal 0-chunk, or
    /// the server closed an until-EOF stream.
    fn finished(&self) -> bool {
        match self.framing {
            RawFraming::Length => self.remaining == 0,
            RawFraming::Chunked => self.chunk == ChunkState::Done,
            RawFraming::UntilEof => self.eof,
        }
    }

    /// Move wire bytes to decoded payload as the framing allows; true when
    /// `decoded` grew (more payload became available).
    fn decode_available(&mut self) -> bool {
        let before = self.decoded.len();
        match self.framing {
            RawFraming::Length => {
                let n = (self.raw.len() as u64).min(self.remaining) as usize;
                if n > 0 {
                    self.remaining -= n as u64;
                    self.decoded.append(&mut self.raw.drain(..n).collect());
                }
            }
            RawFraming::UntilEof => self.decoded.append(&mut std::mem::take(&mut self.raw)),
            RawFraming::Chunked => loop {
                match self.chunk.clone() {
                    ChunkState::Done => break,
                    ChunkState::Data(rem) => {
                        let n = (self.raw.len() as u64).min(rem) as usize;
                        if n == 0 {
                            break;
                        }
                        self.decoded.extend(self.raw.drain(..n));
                        let left = rem - n as u64;
                        self.chunk = if left == 0 {
                            ChunkState::DataCrlf
                        } else {
                            ChunkState::Data(left)
                        };
                    }
                    ChunkState::DataCrlf => {
                        if self.raw.len() < 2 {
                            break;
                        }
                        let skip = usize::from(self.raw[..2] == *b"\r\n") * 2;
                        self.raw.drain(..skip);
                        self.chunk = ChunkState::Size;
                    }
                    ChunkState::Size => {
                        let Some(pos) = crate::framing::find_subsequence(&self.raw, b"\r\n") else {
                            break;
                        };
                        let token = String::from_utf8_lossy(&self.raw[..pos]).into_owned();
                        self.raw.drain(..pos + 2);
                        let size = token
                            .split(';')
                            .next()
                            .unwrap_or("")
                            .trim()
                            .parse::<u64>()
                            .unwrap_or(0);
                        self.chunk = if size == 0 {
                            ChunkState::Done
                        } else {
                            ChunkState::Data(size)
                        };
                    }
                }
            },
        }
        self.decoded.len() > before
    }
}

impl<S: tokio::io::AsyncRead + Unpin> tokio::io::AsyncRead for RawBody<S> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        out: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        if out.remaining() == 0 {
            return std::task::Poll::Ready(Ok(()));
        }
        loop {
            if !self.decoded.is_empty() {
                let n = out.remaining().min(self.decoded.len());
                out.put_slice(&self.decoded[..n]);
                self.decoded.drain(..n);
                return std::task::Poll::Ready(Ok(()));
            }
            if self.finished() {
                return std::task::Poll::Ready(Ok(()));
            }
            if self.decode_available() {
                continue;
            }
            // No more payload can be decoded from what is buffered — pull
            // more wire bytes (scratch-sized; the blob never lands whole).
            let mut scratch = [0u8; 8192];
            let mut rb = tokio::io::ReadBuf::new(&mut scratch);
            match std::pin::Pin::new(&mut self.stream).poll_read(cx, &mut rb) {
                std::task::Poll::Ready(Ok(())) => {
                    let n = rb.filled().len();
                    if n == 0 {
                        self.eof = true;
                        return std::task::Poll::Ready(Ok(()));
                    }
                    self.raw.extend_from_slice(&scratch[..n]);
                }
                other => return other,
            }
        }
    }
}

/// A raw (non-JSON) GET response (T4.13 registry pulls): status plus the
/// headers the pull chain verifies, with the entity streaming lazily.
pub struct RawResponse<S = TcpStream> {
    pub status: u16,
    pub content_type: Option<String>,
    pub docker_content_digest: Option<String>,
    pub content_length: Option<u64>,
    pub body: RawBody<S>,
}

/// One raw `GET` against `host:port` with a custom `Accept` header — the
/// registry-v2 face (T4.13, Q54). NON-2xx statuses are NOT errors here:
/// the [`RawResponse`] carries `status` so the caller maps it (a 404 must
/// be distinguishable from a transport failure); only protocol-level
/// garbage (bad status line, no head) is [`HttpError::BadResponse`]. The
/// body of a dropped error response simply dies with the connection.
pub async fn get_raw(
    host: &str,
    port: u16,
    path: &str,
    accept: &str,
) -> Result<RawResponse, HttpError> {
    let mut stream = tcp_connect(host, port).await?;
    let host_hdr = if port == 80 {
        host.to_string()
    } else {
        format!("{host}:{port}")
    };
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: {host_hdr}\r\nAccept: {accept}\r\nConnection: close\r\n\r\n"
    );
    stream
        .write_all(req.as_bytes())
        .await
        .map_err(|e| HttpError::Io(format!("write request: {e}")))?;
    stream
        .flush()
        .await
        .map_err(|e| HttpError::Io(format!("flush request: {e}")))?;

    let mut head: Vec<u8> = Vec::with_capacity(512);
    let mut buf = [0u8; 1024];
    let boundary = loop {
        if let Some(pos) = crate::framing::find_subsequence(&head, b"\r\n\r\n") {
            break pos + 4;
        }
        let n = tokio::time::timeout(READ_TIMEOUT, stream.read(&mut buf))
            .await
            .map_err(|_| HttpError::Io("timed out reading response head".to_string()))?
            .map_err(|e| HttpError::Io(format!("read response head: {e}")))?;
        if n == 0 {
            return Err(HttpError::BadResponse("closed before response head".into()));
        }
        head.extend_from_slice(&buf[..n]);
    };
    let parsed = parse_raw_head(&head[..boundary])?;
    let body = RawBody::from_parts(
        stream,
        parsed.chunked,
        parsed.content_length,
        head[boundary..].to_vec(),
    );
    Ok(RawResponse {
        status: parsed.status,
        content_type: parsed.content_type,
        docker_content_digest: parsed.docker_content_digest,
        content_length: parsed.content_length,
        body,
    })
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

    #[test]
    fn parse_raw_head_extracts_registry_headers_case_insensitive() {
        let head = b"HTTP/1.1 200 OK\r\nCONTENT-TYPE: application/json\r\n\
                     docker-CONTENT-DIGEST: sha256:abc\r\nContent-Length: 7\r\n\r\n";
        let h = parse_raw_head(head).unwrap();
        assert_eq!(
            (h.status, h.content_length, h.chunked),
            (200, Some(7), false)
        );
        assert_eq!(h.content_type.as_deref(), Some("application/json"));
        assert_eq!(h.docker_content_digest.as_deref(), Some("sha256:abc"));
        let head = b"HTTP/1.1 404 Not Found\r\nTransfer-Encoding: CHUNKED\r\n\r\n";
        let h = parse_raw_head(head).unwrap();
        assert_eq!((h.status, h.content_length, h.chunked), (404, None, true));
        assert_eq!((h.content_type, h.docker_content_digest), (None, None));
        assert!(parse_raw_head(b"garbage").is_err());
    }

    /// RawBody over duplex: canned response bytes, no sockets (mirrors
    /// tests/http_chunked.rs). Writes are best-effort: the reader may stop
    /// at the framing end while bytes are still in flight.
    async fn raw_body_bytes(
        framed: Vec<u8>,
        chunked: bool,
        content_length: Option<u64>,
        leftover: Vec<u8>,
    ) -> Vec<u8> {
        let (mut writer, reader) = tokio::io::duplex(8);
        let mut body = RawBody::from_parts(reader, chunked, content_length, leftover);
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            let _ = writer.write_all(&framed).await;
            let _ = writer.flush().await;
            let _ = writer.shutdown().await;
        });
        body.read_to_end().await.unwrap()
    }

    #[tokio::test]
    async fn raw_body_content_length_streams_and_stops_at_length() {
        // Bytes past Content-Length must never surface.
        let body = raw_body_bytes(
            b"IGNORED-TAIL".to_vec(),
            false,
            Some(8),
            b"payload!".to_vec(),
        )
        .await;
        assert_eq!(body, b"payload!".to_vec());
        let body = raw_body_bytes(Vec::new(), false, Some(0), Vec::new()).await;
        assert!(body.is_empty());
    }

    #[tokio::test]
    async fn raw_body_chunked_decodes_extensions_and_trailers() {
        let framed = b"5;ext=1\r\nhello\r\n6\r\n world\r\n0\r\nX-T: y\r\n\r\n";
        let body = raw_body_bytes(framed.to_vec(), true, None, Vec::new()).await;
        assert_eq!(body, b"hello world".to_vec());
        // A body split across the head leftover and later chunks.
        let leftover = b"5\r\n{\"a\":".to_vec();
        let rest = b"\r\n3\r\n1}\n\r\n0\r\n\r\n".to_vec();
        let body = raw_body_bytes(rest, true, None, leftover).await;
        assert_eq!(body, b"{\"a\":1}\n".to_vec());
    }

    #[tokio::test]
    async fn raw_body_until_eof_reads_everything() {
        let body = raw_body_bytes(Vec::new(), false, None, b"all-of-it".to_vec()).await;
        assert_eq!(body, b"all-of-it".to_vec());
    }
}
