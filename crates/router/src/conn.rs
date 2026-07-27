//! Minimal HTTP/1.1 connection I/O shared by the Lua data plane ([`serve`]) and
//! the reverse proxy ([`proxy`]).
//!
//! Request parsing (head + body framing) and response serialisation live here so
//! both transports reuse one implementation. HTTP/1.1 only; a richer hyper-based
//! transport is revisited in T5.6+. The proxy also reads *upstream* responses,
//! so this module knows how to parse a response head and frame its body.

use bytes::Bytes;
use http::{Method, Request, Response};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Hard cap on a buffered request/response body (1 MiB). Larger bodies are
/// rejected with 413 (true streaming is a T5.6 concern).
pub(crate) const MAX_BODY_BYTES: usize = 1024 * 1024;

// ---------- request parsing ----------

/// How the request body is framed on the wire.
#[derive(Debug)]
pub(crate) enum BodySpec {
    /// No body expected.
    None,
    /// `Content-Length: N`.
    Length(usize),
    /// `Transfer-Encoding: chunked`.
    Chunked,
}

/// Read the request head: bytes up to and including the blank `\r\n` line.
pub(crate) async fn read_head<S: tokio::io::AsyncRead + Unpin>(
    stream: &mut S,
) -> std::io::Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(1024);
    let mut byte = [0u8; 1];
    loop {
        if buf.len() > 65536 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "request head too large",
            ));
        }
        let n = stream.read(&mut byte).await?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionAborted,
                "eof before end of head",
            ));
        }
        buf.push(byte[0]);
        let len = buf.len();
        if len >= 4 && &buf[len - 4..] == b"\r\n\r\n" {
            break;
        }
    }
    Ok(buf)
}

/// Parse the head into a [`Request`] and the body-framing spec.
pub(crate) fn parse_request(head: &[u8]) -> Result<(Request<()>, BodySpec), String> {
    let text = std::str::from_utf8(head).map_err(|_| "non-utf8 request head")?;
    let mut lines = text.split("\r\n");
    let request_line = lines.next().ok_or("empty request")?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().ok_or("missing method")?;
    let target = parts.next().ok_or("missing request target")?;
    let _version = parts.next().ok_or("missing version")?;

    let mut req = Request::builder().method(parse_method(method)?);
    let mut content_length: Option<usize> = None;
    let mut chunked = false;
    for line in lines {
        if line.is_empty() {
            break;
        }
        let (name, value) = line.split_once(':').ok_or("malformed header line")?;
        let name = name.trim();
        let value = value.trim();
        if name.eq_ignore_ascii_case("content-length") {
            content_length = value.parse().ok();
        } else if name.eq_ignore_ascii_case("transfer-encoding")
            && value.eq_ignore_ascii_case("chunked")
        {
            chunked = true;
        }
        if let (Ok(n), Ok(v)) = (
            http::HeaderName::from_bytes(name.as_bytes()),
            http::HeaderValue::from_str(value),
        ) {
            req = req.header(n, v);
        }
    }
    let spec = if chunked {
        BodySpec::Chunked
    } else if let Some(n) = content_length {
        if n == 0 {
            BodySpec::None
        } else {
            BodySpec::Length(n)
        }
    } else {
        BodySpec::None
    };
    let req = req
        .uri(target)
        .body(())
        .map_err(|e| format!("bad request: {e}"))?;
    Ok((req, spec))
}

fn parse_method(m: &str) -> Result<Method, String> {
    Method::from_bytes(m.as_bytes()).map_err(|_| format!("bad method {m}"))
}

/// Error reading a framed body.
pub(crate) enum BodyError {
    Io(std::io::Error),
    TooLarge,
}

/// Read the request body per [`BodySpec`], enforcing [`MAX_BODY_BYTES`].
pub(crate) async fn read_body<S: tokio::io::AsyncRead + Unpin>(
    stream: &mut S,
    spec: BodySpec,
) -> Result<Vec<u8>, BodyError> {
    match spec {
        BodySpec::None => Ok(Vec::new()),
        BodySpec::Length(n) => {
            if n > MAX_BODY_BYTES {
                return Err(BodyError::TooLarge);
            }
            let mut buf = vec![0u8; n];
            stream.read_exact(&mut buf).await.map_err(BodyError::Io)?;
            Ok(buf)
        }
        BodySpec::Chunked => read_chunked(stream).await,
    }
}

/// Decode HTTP/1.1 chunked transfer-encoding into a flat buffer.
async fn read_chunked<S: tokio::io::AsyncRead + Unpin>(
    stream: &mut S,
) -> Result<Vec<u8>, BodyError> {
    let mut out = Vec::new();
    loop {
        let size_line = read_line(stream).await.map_err(BodyError::Io)?;
        let size_str = size_line.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_str, 16).map_err(|_| {
            BodyError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "bad chunk size",
            ))
        })?;
        if size == 0 {
            let _ = read_line(stream).await; // trailing CRLF after last-chunk.
            break;
        }
        if out.len() + size > MAX_BODY_BYTES {
            return Err(BodyError::TooLarge);
        }
        let prev = out.len();
        out.resize(prev + size, 0u8);
        stream
            .read_exact(&mut out[prev..])
            .await
            .map_err(BodyError::Io)?;
        let mut crlf = [0u8; 2];
        stream.read_exact(&mut crlf).await.map_err(BodyError::Io)?;
    }
    Ok(out)
}

/// Read one CRLF-terminated line (the CRLF is not included in the result).
pub(crate) async fn read_line<S: tokio::io::AsyncRead + Unpin>(
    stream: &mut S,
) -> std::io::Result<String> {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let n = stream.read(&mut byte).await?;
        if n == 0 {
            break;
        }
        if byte[0] == b'\n' {
            if buf.last() == Some(&b'\r') {
                buf.pop();
            }
            break;
        }
        buf.push(byte[0]);
    }
    String::from_utf8(buf).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

// ---------- response serialisation ----------

/// Serialise an [`http::Response`] onto the wire (HTTP/1.1, connection: close).
pub(crate) async fn write_response<S: tokio::io::AsyncWrite + Unpin>(
    stream: &mut S,
    resp: Response<Bytes>,
) -> std::io::Result<()> {
    let (parts, body) = resp.into_parts();
    let status = parts.status.as_u16();
    let reason = reason_phrase(status);
    let mut out = Vec::with_capacity(body.len() + 256);
    out.extend_from_slice(format!("HTTP/1.1 {status} {reason}\r\n").as_bytes());
    for (name, value) in &parts.headers {
        out.extend_from_slice(name.as_str().as_bytes());
        out.extend_from_slice(b": ");
        out.extend_from_slice(value.as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b"connection: close\r\n\r\n");
    out.extend_from_slice(&body);
    stream.write_all(&out).await?;
    stream.flush().await?;
    Ok(())
}

pub(crate) fn error_response(status: u16, msg: &str) -> Response<Bytes> {
    let body = Bytes::from(msg.as_bytes().to_vec());
    Response::builder()
        .status(http::StatusCode::from_u16(status).unwrap_or(http::StatusCode::BAD_REQUEST))
        .header(http::header::CONTENT_TYPE, "text/plain")
        .header(http::header::CONTENT_LENGTH, body.len())
        .body(body)
        .unwrap()
}

pub(crate) fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        301 => "Moved Permanently",
        302 => "Found",
        303 => "See Other",
        307 => "Temporary Redirect",
        308 => "Permanent Redirect",
        304 => "Not Modified",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        413 => "Payload Too Large",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "OK",
    }
}

// ---------- upstream response parsing (proxy) ----------

/// A parsed upstream HTTP response: status + headers + body bytes.
pub(crate) struct UpstreamResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// Read an entire upstream response: send with `Connection: close`, then read
/// everything until EOF and split head/body. This is simple and robust for the
/// proxy; keep-alive upstream pooling is a Scope B concern.
pub(crate) async fn read_upstream_response<S: tokio::io::AsyncRead + Unpin>(
    stream: &mut S,
) -> std::io::Result<UpstreamResponse> {
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await?;
    parse_upstream_response(&buf)
}

/// Parse `HTTP/1.1 STATUS REASON\r\n..headers..\r\n\r\nBODY` from a buffer.
fn parse_upstream_response(buf: &[u8]) -> std::io::Result<UpstreamResponse> {
    // Find the head/body boundary.
    let split = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "no response head"))?;
    let head = std::str::from_utf8(&buf[..split])
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "non-utf8 head"))?;
    let body = buf[split + 4..].to_vec();
    if body.len() > MAX_BODY_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "upstream response body too large",
        ));
    }

    let mut lines = head.split("\r\n");
    let status_line = lines.next().ok_or_else(|| io_err("empty status line"))?;
    let status = parse_status_line(status_line)?;
    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_owned(), value.trim().to_owned()));
        }
    }
    Ok(UpstreamResponse {
        status,
        headers,
        body,
    })
}

fn parse_status_line(line: &str) -> std::io::Result<u16> {
    // `HTTP/1.1 200 OK`
    let mut parts = line.split_whitespace();
    let _ver = parts.next();
    let code = parts
        .next()
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or_else(|| io_err("bad status line"))?;
    Ok(code)
}

fn io_err(msg: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, msg)
}

/// RFC 7230 hop-by-hop headers that must not be forwarded by a proxy.
pub(crate) fn is_hop_by_hop(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_request_specs() {
        let (req, spec) =
            parse_request(b"POST /a HTTP/1.1\r\nHost: x\r\nContent-Length: 5\r\n\r\n").unwrap();
        assert_eq!(req.method(), Method::POST);
        assert!(matches!(spec, BodySpec::Length(5)));
        let (_, spec) =
            parse_request(b"POST /a HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n")
                .unwrap();
        assert!(matches!(spec, BodySpec::Chunked));
        let (_, spec) = parse_request(b"GET /a HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
        assert!(matches!(spec, BodySpec::None));
    }

    #[test]
    fn parse_upstream_response_body() {
        let buf = b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\n\r\nhello";
        let r = parse_upstream_response(buf).unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.body, b"hello");
        assert_eq!(r.headers[0].0, "Content-Type");
    }

    #[test]
    fn hop_by_hop_detection() {
        assert!(is_hop_by_hop("Connection"));
        assert!(is_hop_by_hop("transfer-encoding"));
        assert!(!is_hop_by_hop("content-type"));
    }
}
