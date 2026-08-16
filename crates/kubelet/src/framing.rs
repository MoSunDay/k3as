//! HTTP/1.1 response framing: status/header parsing + whole-buffer body
//! decoding (TODO **T4.2**). Pure functions over byte slices, shared by the
//! one-shot request path of [`crate::http`]; the incremental watch-stream
//! decoder lives on `WatchConn` itself.

/// Failure type reused from the client module (errors as values).
pub type FError = crate::http::HttpError;

/// First index of `needle` in `haystack` (byte-exact, no UTF-8 assumptions).
pub fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Parse status line + headers: `(code, content_length, chunked)`.
pub fn parse_head(head: &[u8]) -> Result<(u16, Option<u64>, bool), FError> {
    let text = String::from_utf8_lossy(head);
    let mut lines = text.lines();
    let status = lines
        .next()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| FError::BadResponse("empty response head".to_string()))?;
    let mut parts = status.split_whitespace();
    let version = parts.next().unwrap_or("");
    let code: u16 = parts
        .next()
        .and_then(|c| c.parse().ok())
        .ok_or_else(|| FError::BadResponse(format!("bad status line {status:?}")))?;
    if !version.starts_with("HTTP/") {
        return Err(FError::BadResponse(format!("bad HTTP version {version:?}")));
    }
    let mut content_length = None;
    let mut chunked = false;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        match name.trim().to_ascii_lowercase().as_str() {
            "content-length" => content_length = value.trim().parse::<u64>().ok(),
            "transfer-encoding" => chunked = value.to_ascii_lowercase().contains("chunked"),
            _ => {}
        }
    }
    Ok((code, content_length, chunked))
}

/// `(body_start, content_length)` once headers are complete; `None` length
/// means chunked or read-to-EOF (wait for the server close).
pub fn response_shape(raw: &[u8]) -> Option<(usize, Option<u64>)> {
    let boundary = find_subsequence(raw, b"\r\n\r\n")? + 4;
    let (_code, len, chunked) = parse_head(&raw[..boundary]).ok()?;
    Some((boundary, if chunked { None } else { len }))
}

/// Parse one fully buffered response into `(code, decoded body bytes)`.
pub fn parse_response(raw: &[u8]) -> Result<(u16, Vec<u8>), FError> {
    let boundary = find_subsequence(raw, b"\r\n\r\n")
        .ok_or_else(|| FError::BadResponse("response headers never completed".to_string()))?
        + 4;
    let (code, content_length, chunked) = parse_head(&raw[..boundary])?;
    let body_raw = &raw[boundary..];
    let body = if chunked {
        decode_chunked_all(body_raw)?
    } else if let Some(n) = content_length {
        if body_raw.len() < n as usize {
            return Err(FError::BadResponse(format!(
                "truncated body: got {} of {n} bytes",
                body_raw.len()
            )));
        }
        body_raw[..n as usize].to_vec()
    } else {
        body_raw.to_vec()
    };
    Ok((code, body))
}

/// Whole-buffer chunked decode: `<hex-size>[;ext]\r\n<data>\r\n` repeated,
/// `0\r\n` then optional trailers (ignored) terminated by a blank line/EOF.
pub fn decode_chunked_all(body: &[u8]) -> Result<Vec<u8>, FError> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    loop {
        let line_end = find_subsequence(&body[pos..], b"\r\n")
            .map(|p| p + pos)
            .ok_or_else(|| FError::BadResponse("chunked body: missing size line".to_string()))?;
        let token = String::from_utf8_lossy(&body[pos..line_end]);
        let hex = token.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(hex, 16)
            .map_err(|_| FError::BadResponse(format!("chunked body: bad size {token:?}")))?;
        pos = line_end + 2;
        if size == 0 {
            return Ok(out);
        }
        let end = pos
            .checked_add(size)
            .filter(|e| *e <= body.len())
            .ok_or_else(|| FError::BadResponse("chunked body: truncated chunk".to_string()))?;
        out.extend_from_slice(&body[pos..end]);
        pos = end;
        if body.len() >= pos + 2 && body[pos..pos + 2] == *b"\r\n" {
            pos += 2;
        } else if pos != body.len() {
            return Err(FError::BadResponse(
                "chunked body: missing CRLF".to_string(),
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_head_status_and_headers() {
        let (code, len, chunked) =
            parse_head(b"HTTP/1.1 200 OK\r\nContent-Length: 7\r\n\r\n").unwrap();
        assert_eq!((code, len, chunked), (200, Some(7), false));
        let (code, len, chunked) =
            parse_head(b"HTTP/1.1 404 Not Found\r\nTransfer-Encoding: chunked\r\n\r\n").unwrap();
        assert_eq!((code, len, chunked), (404, None, true));
        // Header names are case-insensitive.
        let (_, len, _) = parse_head(b"HTTP/1.1 200 OK\r\ncontent-LENGTH: 3\r\n\r\n").unwrap();
        assert_eq!(len, Some(3));
        assert!(parse_head(b"garbage").is_err());
        assert!(parse_head(b"HTTP/1.1 xyz bad\r\n\r\n").is_err());
        assert!(parse_head(b"FTP/1.0 200 OK\r\n\r\n").is_err());
    }

    #[test]
    fn parse_response_content_length_and_empty() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 7\r\n\r\n{\"a\":1}";
        let (code, body) = parse_response(raw).unwrap();
        assert_eq!((code, body), (200, b"{\"a\":1}".to_vec()));
        let raw = b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n";
        let (code, body) = parse_response(raw).unwrap();
        assert_eq!((code, body), (404, Vec::new()));
        // Extra bytes beyond Content-Length are ignored.
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nokEXTRA";
        assert_eq!(parse_response(raw).unwrap().1, b"ok".to_vec());
    }

    #[test]
    fn parse_response_chunked_with_extensions_and_trailers() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\n{\"a\":\r\n2\r\n1}\r\n0\r\n\r\n";
        assert_eq!(parse_response(raw).unwrap().1, b"{\"a\":1}".to_vec());
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n7;ext=1\r\n{\"a\":2}\r\n0\r\nX-T: y\r\n\r\n";
        assert_eq!(parse_response(raw).unwrap().1, b"{\"a\":2}".to_vec());
    }

    #[test]
    fn parse_response_rejects_garbage() {
        assert!(parse_response(b"not http at all").is_err());
        assert!(parse_response(b"HTTP/1.1 200 OK\r\nContent-Length: 99\r\n\r\n{\"a\":1}").is_err());
        assert!(
            parse_response(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n9\r\nab")
                .is_err()
        );
    }

    #[test]
    fn response_shape_reports_completeness() {
        let head = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\n";
        assert_eq!(response_shape(head), Some((head.len(), Some(5))));
        let chunked = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4\r\nxx";
        let boundary = find_subsequence(chunked, b"\r\n\r\n").unwrap() + 4;
        assert_eq!(response_shape(chunked), Some((boundary, None)));
        assert_eq!(response_shape(b"HTTP/1.1 200 OK"), None);
    }

    #[test]
    fn decode_chunked_all_partial_and_malformed() {
        assert_eq!(decode_chunked_all(b"0\r\n\r\n").unwrap(), Vec::<u8>::new());
        assert!(decode_chunked_all(b"zz\r\n").is_err());
        assert!(decode_chunked_all(b"4\r\nab").is_err());
    }
}
