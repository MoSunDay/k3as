//! Minimal HTTP/1.1 client for the kubelet loops (TODO **T4.2**, Q21 parity).
//!
//! Same dependency-free posture as `kubectl`'s transport: plain `http://`
//! only (https arrives with T1.3), one `Connection: close` request per call,
//! and a hand-rolled chunked decoder -- required here because the apiserver
//! streams watch events as chunked `application/json`, one event per line.
//! [`WatchConn`] is generic over the read half so the incremental decoder is
//! testable over `tokio::io::duplex` without sockets (see `tests/`).
//!
//! Since T4.13 (Q54) the module also carries the raw-GET face the image
//! puller speaks: [`get_raw`] returns status + registry headers with the
//! entity streaming lazily off the owned connection ([`RawBody`]), so blobs
//! never buffer whole in memory.
//!
//! Split by responsibility (C2): [`client`] is the JSON/watch face
//! ([`HttpJson`]), [`body`] is the raw registry face ([`get_raw`] +
//! [`RawBody`]) plus the bounded-read helpers, and `tests` holds the module's
//! unit tests. Every read is time-bounded: a stalled registry must fail the
//! pull, never hang the sync worker.

use std::time::Duration;

use tokio::net::TcpStream;

mod body;
mod client;
#[cfg(test)]
mod tests;

pub use body::{get_raw, read_body_with_deadline, RawBody, RawResponse};
pub use client::HttpJson;

/// Refused/unroutable servers must fail fast.
pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Per-read timeout for one request/response exchange (response heads and
/// small JSON bodies).
pub(crate) const READ_TIMEOUT: Duration = Duration::from_secs(10);

/// C2: per-read timeout for a STREAMING entity (registry blobs). Wider than
/// [`READ_TIMEOUT`] on purpose -- 30s without a single wire chunk means the
/// registry is dead, while a slow-but-flowing blob must keep going. The
/// overall budget of one pull lives in `engine::image::pull::PULL_DEADLINE`.
pub const BODY_READ_TIMEOUT: Duration = Duration::from_secs(30);

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

/// Connect with the shared fast-fail budget (refused/unroutable servers).
async fn tcp_connect(host: &str, port: u16) -> Result<TcpStream, HttpError> {
    let addr = (host, port);
    tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(addr))
        .await
        .map_err(|_| HttpError::Connect(format!("connect to {host}:{port} timed out")))?
        .map_err(|e| HttpError::Connect(format!("connect to {host}:{port} failed: {e}")))
}
