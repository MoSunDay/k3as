//! Outbound kubelet hop transport for the T4.2 Scope C relay (`pods/log`,
//! `pods/exec`). Split out of [`crate::pod_io`] to respect the <=400-line
//! new-file budget; this module owns ONLY bytes-in/bytes-out:
//!
//!   - [`build_hop_request`] pins the frozen wire contract byte-for-byte
//!     (`GET|POST /pods/<uid>/containers/<c>/log|exec`, `Connection: close`);
//!   - [`call_kubelet`] enforces the Q21-style constraints: one TCP
//!     connection per hop over `tokio::net::TcpStream`, 5s timeout per hop,
//!     32 MiB response cap;
//!   - framing handles Content-Length / chunked / read-to-EOF (the same
//!     shapes crates/kubectl/src/http.rs already solves);
//!   - no new dependencies anywhere (Q21); log bodies stay text/plain on
//!     pass-through — decision Q31.

use std::time::Duration;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::error::status_failure;

/// Per-hop timeout (connect + write + read) for the kubelet relay.
pub(crate) const HOP_TIMEOUT: Duration = Duration::from_secs(5);
/// Hard cap on one relayed response (matches the kubectl client cap).
pub(crate) const MAX_RESPONSE_BYTES: usize = 32 * 1024 * 1024;

/// Build the exact HTTP/1.1 request bytes for one hop (pure; unit-tested —
/// this pins the frozen wire contract byte-for-byte).
pub(crate) fn build_hop_request(
    method: &str,
    target: &str,
    host_port: &str,
    content_type: Option<&str>,
    body: Option<&[u8]>,
) -> Vec<u8> {
    let mut head =
        format!("{method} {target} HTTP/1.1\r\nHost: {host_port}\r\nConnection: close\r\n");
    match (content_type, body) {
        (Some(ct), Some(payload)) => head.push_str(&format!(
            "Accept: application/json\r\nContent-Type: {ct}\r\nContent-Length: {}\r\n",
            payload.len()
        )),
        _ => head.push_str("Accept: */*\r\n"),
    }
    head.push_str("\r\n");
    let mut out = head.into_bytes();
    out.extend_from_slice(body.unwrap_or_default());
    out
}

/// One parsed kubelet reply ready for verbatim pass-through.
pub(crate) struct KubeletReply {
    pub(crate) code: u16,
    /// Lowercased Content-Type of the kubelet answer (absent -> `None`).
    pub(crate) content_type: Option<String>,
    pub(crate) body: Vec<u8>,
}

/// Split one raw hop response into status/content-type/body.
pub(crate) fn parse_hop_response(raw: &[u8]) -> Result<KubeletReply, String> {
    let hdr_end = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or("malformed response: no header terminator")?;
    let headers = std::str::from_utf8(&raw[..hdr_end])
        .map_err(|_| "malformed response: non-UTF-8 headers".to_string())?;
    let mut lines = headers.split("\r\n");
    let code = lines
        .next()
        .unwrap_or_default()
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| format!("malformed status line {headers:?}"))?;

    let mut chunked = false;
    let mut content_length: Option<usize> = None;
    let mut content_type: Option<String> = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim().to_ascii_lowercase();
        let value = value.trim();
        match name.as_str() {
            "content-length" => content_length = value.parse::<usize>().ok(),
            "transfer-encoding" if value.to_ascii_lowercase().contains("chunked") => {
                chunked = true;
            }
            "content-type" => content_type = Some(value.to_ascii_lowercase()),
            _ => {}
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
    Ok(KubeletReply {
        code,
        content_type,
        body,
    })
}

/// Total wire length once a Content-Length-framed response is complete;
/// `None` keeps the read loop waiting for EOF (chunked / length-less).
fn framed_response_len(raw: &[u8]) -> Option<usize> {
    let hdr_end = raw.windows(4).position(|w| w == b"\r\n\r\n")? + 4;
    let headers = std::str::from_utf8(&raw[..hdr_end]).ok()?;
    let n = headers.split("\r\n").skip(1).find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.trim()
            .eq_ignore_ascii_case("content-length")
            .then(|| value.trim().parse::<usize>().ok())?
    })?;
    Some(hdr_end + n)
}

/// Decode chunked framing `<hex>[;ext]\r\n<data>\r\n`... `0\r\n` + trailers
/// (same decoder as crates/kubectl/src/http.rs — shared Q21 discipline).
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

/// One kubelet hop: build + send + frame-parse, capped by [`HOP_TIMEOUT`] and
/// [`MAX_RESPONSE_BYTES`]. Every failure surfaces as `Err(String)` which the
/// caller renders as 502 `relay to kubelet failed: <err>`.
pub(crate) async fn call_kubelet(
    ip: &str,
    port: u16,
    method: &str,
    request_target: &str,
    content_type: Option<&str>,
    body: Option<Vec<u8>>,
) -> Result<KubeletReply, String> {
    let host_port = format!("{ip}:{port}");
    let request = build_hop_request(
        method,
        request_target,
        &host_port,
        content_type,
        body.as_deref(),
    );
    let raw = tokio::time::timeout(HOP_TIMEOUT, hop_exchange((ip.to_string(), port), request))
        .await
        .map_err(|_| format!("timed out after {}s", HOP_TIMEOUT.as_secs()))??;
    parse_hop_response(&raw)
}

/// Connect, write the whole request, read until EOF or Content-Length is
/// satisfied (chunked bodies ride until EOF), enforcing the response cap.
async fn hop_exchange(addr: (String, u16), request: Vec<u8>) -> Result<Vec<u8>, String> {
    let mut stream = TcpStream::connect(addr)
        .await
        .map_err(|e| format!("connect failed: {e}"))?;
    stream
        .write_all(&request)
        .await
        .map_err(|e| format!("write request failed: {e}"))?;
    stream
        .flush()
        .await
        .map_err(|e| format!("flush failed: {e}"))?;

    let mut raw: Vec<u8> = Vec::with_capacity(4096);
    let mut chunk = [0u8; 8192];
    loop {
        if framed_response_len(&raw).is_some_and(|total| raw.len() >= total) {
            break;
        }
        let n = stream
            .read(&mut chunk)
            .await
            .map_err(|e| format!("read response failed: {e}"))?;
        if n == 0 {
            break; // EOF (we asked for Connection: close)
        }
        raw.extend_from_slice(&chunk[..n]);
        if raw.len() > MAX_RESPONSE_BYTES {
            return Err(format!(
                "response exceeds {} MiB limit",
                MAX_RESPONSE_BYTES >> 20
            ));
        }
    }
    Ok(raw)
}

/// Render a hop outcome: transport failures become the contract's 502 Status
/// envelope, successes pass code/body/Content-Type through VERBATIM (Q31).
pub(crate) async fn relay_reply(hop: Result<KubeletReply, String>) -> Response {
    let reply = match hop {
        Ok(reply) => reply,
        Err(err) => {
            return status_failure(
                StatusCode::BAD_GATEWAY,
                "ServiceUnavailable",
                format!("relay to kubelet failed: {err}"),
            )
        }
    };
    let status = StatusCode::from_u16(reply.code).unwrap_or(StatusCode::BAD_GATEWAY);
    use axum::http::{header, HeaderValue};
    let mut response = (status, reply.body).into_response();
    if let Some(ct) = reply.content_type.filter(|ct| !ct.is_empty()) {
        if let Ok(value) = HeaderValue::from_str(&ct) {
            response.headers_mut().insert(header::CONTENT_TYPE, value);
        }
    }
    response
}
