//! Incremental watch-stream decoder (TODO **T4.2**).
//!
//! The apiserver streams watch events as chunked `application/json`, one
//! `{"type","object"}` line per event. [`WatchConn`] wraps any
//! `AsyncRead` and yields complete lines as the framing allows: chunked
//! transfer encoding wins, then a known Content-Length, else read-to-EOF.
//! Cancel-safe: dropping a pending [`WatchConn::next_line`] future loses no
//! buffered state. Generic over the reader so tests can drive it over
//! `tokio::io::duplex` (see `tests/http_chunked.rs`).

use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::net::TcpStream;

use crate::framing::find_subsequence;

/// Upper bound for a single watch body read; the stream itself is unbounded.
const WATCH_READ_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// Body framing of an open stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BodyMode {
    Chunked,
    Length(u64),
    UntilEof,
}

/// Incremental state of the chunked decoder.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ChunkState {
    Size,
    Data(u64),
    DataCrlf,
    Done,
}

/// An open watch stream: reads incrementally, decodes framing, and yields
/// complete lines (partial lines are buffered across reads).
pub struct WatchConn<S = TcpStream> {
    stream: S,
    mode: BodyMode,
    chunk: ChunkState,
    pub(crate) raw: Vec<u8>,
    line: Vec<u8>,
    eof: bool,
}

impl<S: AsyncRead + Unpin> WatchConn<S> {
    /// Build a watch connection over any reader: `chunked` framing wins,
    /// then a known `content_length`, else read-to-EOF.
    pub fn from_parts(stream: S, chunked: bool, content_length: Option<u64>) -> Self {
        let mode = if chunked {
            BodyMode::Chunked
        } else if let Some(len) = content_length {
            BodyMode::Length(len)
        } else {
            BodyMode::UntilEof
        };
        Self {
            stream,
            mode,
            chunk: ChunkState::Size,
            raw: Vec::new(),
            line: Vec::new(),
            eof: false,
        }
    }

    /// Next complete line, or `None` once the body is exhausted/closed.
    /// Cancel-safe: dropping the future mid-read loses no buffered state.
    pub async fn next_line(&mut self) -> Option<String> {
        loop {
            if let Some(pos) = self.line.iter().position(|&b| b == b'\n') {
                let mut out: Vec<u8> = self.line.drain(..=pos).collect();
                out.pop();
                if out.last() == Some(&b'\r') {
                    out.pop();
                }
                return Some(String::from_utf8_lossy(&out).into_owned());
            }
            if self.eof {
                // A split CRLF at disconnect must not surface as a line.
                if self.line.last() == Some(&b'\r') {
                    self.line.pop();
                }
                if self.line.is_empty() {
                    return None;
                }
                let out = std::mem::take(&mut self.line);
                return Some(String::from_utf8_lossy(&out).into_owned());
            }
            let mut buf = [0u8; 4096];
            match tokio::time::timeout(WATCH_READ_TIMEOUT, self.stream.read(&mut buf)).await {
                Ok(Ok(0)) | Ok(Err(_)) | Err(_) => {
                    self.eof = true;
                    self.decode_available();
                }
                Ok(Ok(n)) => {
                    self.raw.extend_from_slice(&buf[..n]);
                    self.decode_available();
                }
            }
        }
    }

    /// Consume as much of `raw` as the framing allows, appending decoded
    /// payload bytes to `line`.
    pub(crate) fn decode_available(&mut self) {
        loop {
            if self.eof {
                return;
            }
            match self.mode {
                BodyMode::UntilEof => {
                    self.line.append(&mut self.raw);
                    return;
                }
                BodyMode::Length(rem) => {
                    let n = (self.raw.len() as u64).min(rem) as usize;
                    self.line.extend(self.raw.drain(..n));
                    self.mode = BodyMode::Length(rem - n as u64);
                    if rem == n as u64 {
                        self.eof = true;
                    }
                    return;
                }
                BodyMode::Chunked => {
                    if !self.decode_chunk_step() {
                        return;
                    }
                }
            }
        }
    }

    /// One step of the chunked state machine. `true` = made progress (loop
    /// again), `false` = need more input (or done).
    fn decode_chunk_step(&mut self) -> bool {
        match self.chunk.clone() {
            ChunkState::Size => {
                let Some(pos) = find_subsequence(&self.raw, b"\r\n") else {
                    return false;
                };
                let token = String::from_utf8_lossy(&self.raw[..pos]).into_owned();
                let hex = token.split(';').next().unwrap_or("").trim().to_string();
                self.raw.drain(..pos + 2);
                match usize::from_str_radix(&hex, 16) {
                    Ok(0) => {
                        self.chunk = ChunkState::Done;
                        self.eof = true; // trailers (if any) are ignored
                    }
                    Ok(n) => self.chunk = ChunkState::Data(n as u64),
                    Err(_) => self.eof = true, // malformed framing: stop
                }
                true
            }
            ChunkState::Data(rem) => {
                if self.raw.is_empty() {
                    return false;
                }
                let n = (self.raw.len() as u64).min(rem) as usize;
                self.line.extend(self.raw.drain(..n));
                let rem = rem - n as u64;
                self.chunk = if rem == 0 {
                    ChunkState::DataCrlf
                } else {
                    ChunkState::Data(rem)
                };
                true
            }
            ChunkState::DataCrlf => {
                if self.raw.len() < 2 {
                    return false;
                }
                let skip = usize::from(self.raw[..2] == *b"\r\n") * 2;
                self.raw.drain(..skip);
                self.chunk = ChunkState::Size;
                true
            }
            ChunkState::Done => {
                self.eof = true;
                false
            }
        }
    }
}
