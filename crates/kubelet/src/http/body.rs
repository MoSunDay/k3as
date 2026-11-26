//! T4.13 (Q54): the raw-GET registry face of the kubelet's minimal HTTP/1.1
//! client -- [`get_raw`] hands back status + registry headers with the entity
//! streaming lazily off the owned connection ([`RawBody`]), so a blob never
//! buffers whole in memory.
//!
//! C2: every read off that entity is time-bounded ([`RawBody::read_chunk`]
//! per read, [`read_body_with_deadline`] for the whole entity) -- a registry
//! that accepts the connection and then stalls used to park the sync worker
//! forever. Timeouts are `io::ErrorKind::TimedOut` with "timed out" in the
//! message, so the pull chain maps them onto its own error type.

use std::io;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::framing::find_subsequence;

use super::{tcp_connect, HttpError, BODY_READ_TIMEOUT, READ_TIMEOUT};

/// A read that exceeded its budget, as the `TimedOut` `io::Error` the
/// callers already know how to map (`"timed out ..."` in the message).
fn timed_out(what: &str, budget: Duration) -> io::Error {
    io::Error::new(
        io::ErrorKind::TimedOut,
        format!("timed out reading {what} after {budget:?}"),
    )
}

// ================= raw GET face (T4.13 image pull, Q54) ====================

/// Parsed raw response head fields (see [`parse_raw_head`]).
pub(super) struct RawHead {
    pub(super) status: u16,
    pub(super) content_length: Option<u64>,
    pub(super) chunked: bool,
    pub(super) content_type: Option<String>,
    pub(super) docker_content_digest: Option<String>,
}

/// Parse a raw response head: `(status, content_length, chunked,
/// content_type, docker_content_digest)`. Header names are case-insensitive.
pub(super) fn parse_raw_head(head: &[u8]) -> Result<RawHead, HttpError> {
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
    /// entity, one bounded read at a time (C2). Blobs must stream through
    /// [`AsyncRead`] or [`Self::read_chunk`] instead. An overall deadline
    /// comes from [`read_body_with_deadline`]; a byte cap from
    /// [`Self::read_to_end_capped`] (D).
    pub async fn read_to_end(&mut self) -> std::io::Result<Vec<u8>> {
        self.read_to_end_capped(usize::MAX).await
    }

    /// [`Self::read_to_end`] under a byte `cap` (D): an untrusted registry
    /// cannot size the kubelet's heap -- `InvalidData` once the bytes pass `cap`.
    pub async fn read_to_end_capped(&mut self, cap: usize) -> std::io::Result<Vec<u8>> {
        let mut out = Vec::new();
        let mut buf = [0u8; 8192];
        loop {
            let n = self.read_chunk(&mut buf).await?;
            if n == 0 {
                return Ok(out);
            }
            if out.len() + n > cap {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("response body exceeds the {cap}-byte cap"),
                ));
            }
            out.extend_from_slice(&buf[..n]);
        }
    }

    /// One read bounded by [`BODY_READ_TIMEOUT`] (C2): a registry that stops
    /// sending mid-blob fails here instead of hanging the pull worker.
    pub async fn read_chunk(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.read_chunk_within(buf, BODY_READ_TIMEOUT).await
    }

    /// [`Self::read_chunk`] with an explicit per-read budget -- the seam the
    /// unit tests use to prove the timeout without sleeping for 30s.
    pub async fn read_chunk_within(
        &mut self,
        buf: &mut [u8],
        budget: Duration,
    ) -> std::io::Result<usize> {
        tokio::time::timeout(budget, AsyncReadExt::read(self, buf))
            .await
            .map_err(|_| timed_out("response body", budget))?
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
                        let Some(pos) = find_subsequence(&self.raw, b"\r\n") else {
                            break;
                        };
                        let token = String::from_utf8_lossy(&self.raw[..pos]).into_owned();
                        self.raw.drain(..pos + 2);
                        // D: chunk-size is HEX (RFC 7230) -- "1a" is 26
                        // bytes, not a parse failure that ends the body.
                        let size =
                            u64::from_str_radix(token.split(';').next().unwrap_or("").trim(), 16)
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
/// `extra_headers` (T4.13 M5) rides after `Host`/`Accept` — the pull
/// chain passes its env-var `Authorization` header through verbatim.
pub async fn get_raw(
    host: &str,
    port: u16,
    path: &str,
    accept: &str,
    extra_headers: &[(&str, String)],
) -> Result<RawResponse, HttpError> {
    let mut stream = tcp_connect(host, port).await?;
    let host_hdr = if port == 80 {
        host.to_string()
    } else {
        format!("{host}:{port}")
    };
    let mut req = format!("GET {path} HTTP/1.1\r\nHost: {host_hdr}\r\nAccept: {accept}\r\n");
    for (name, value) in extra_headers {
        req.push_str(name);
        req.push_str(": ");
        req.push_str(value);
        req.push_str("\r\n");
    }
    req.push_str("Connection: close\r\n\r\n");
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
        if let Some(pos) = find_subsequence(&head, b"\r\n\r\n") {
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
/// Read `body` to end under an overall `deadline` (C2): every single read
/// keeps the per-read [`BODY_READ_TIMEOUT`], and the WHOLE entity must land
/// before `deadline`. Either bound expiring yields `io::ErrorKind::TimedOut`
/// with "timed out" in the message. Generic over the reader so tests drive it
/// over `tokio::io::duplex` without sockets.
pub async fn read_body_with_deadline<R: AsyncRead + Unpin>(
    body: &mut R,
    deadline: tokio::time::Instant,
) -> std::io::Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            return Err(timed_out("response body", Duration::ZERO));
        }
        let per_read = left.min(BODY_READ_TIMEOUT);
        let n = tokio::time::timeout(per_read, body.read(&mut buf))
            .await
            .map_err(|_| timed_out("response body", per_read))?;
        let n = n?;
        if n == 0 {
            return Ok(out);
        }
        out.extend_from_slice(&buf[..n]);
    }
}
