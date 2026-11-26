//! `http` module tests (T4.2/T4.13): URL + request-head shaping, the raw
//! registry head parser, chunked/length/until-EOF entity framing over
//! `tokio::io::duplex` (no sockets), and since C2 the bounded-read seams
//! (`read_chunk_within`, `read_body_with_deadline`) -- a stalled reader must
//! fail fast, never hang the caller.

use std::time::Duration;

use super::body::parse_raw_head;
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
fn token_stamps_authorization_on_every_head() {
    // T1.3a (Q47 P3): `--token` passthrough — the header lands on both
    // the request and the watch face (both funnel through
    // `request_head_ct`), and stays absent when no token is set.
    let plain = HttpJson::parse_url("http://127.0.0.1:6443").unwrap();
    assert!(!plain
        .request_head("GET", "/api/v1/pods", None)
        .contains("Authorization:"));
    let authed = plain.with_token(Some("s3cr3t".to_string()));
    let req = authed.request_head("GET", "/api/v1/nodes/n1", None);
    assert!(req.contains("Authorization: Bearer s3cr3t\r\n"));
    let put = authed.request_head_ct(
        "PATCH",
        "/api/v1/pods/p",
        Some("{}"),
        "application/merge-patch+json",
    );
    assert!(put.contains("Authorization: Bearer s3cr3t\r\n"));
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

/// A reader that NEVER yields a byte and never closes: the shape of a
/// registry that accepted the connection and then stalled (C2).
fn stalled_body() -> RawBody<tokio::io::DuplexStream> {
    let (_writer, reader) = tokio::io::duplex(8);
    // `_writer` stays alive in the caller's frame, so no EOF ever arrives.
    RawBody::from_parts(reader, false, None, Vec::new())
}

#[tokio::test]
async fn read_chunk_times_out_on_a_stalled_body() {
    let (_writer, reader) = tokio::io::duplex(8);
    let mut body = RawBody::from_parts(reader, false, None, Vec::new());
    let mut buf = [0u8; 16];
    let started = std::time::Instant::now();
    let err = body
        .read_chunk_within(&mut buf, Duration::from_millis(20))
        .await
        .expect_err("a stalled read must not block forever");
    assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
    assert!(err.to_string().contains("timed out"), "{err}");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the bound is the budget, not the default 30s"
    );
    let _ = stalled_body(); // keep the shape helper compiled & exercised
}

#[tokio::test]
async fn read_body_with_deadline_fails_fast_and_keeps_a_flowing_body() {
    // Stalled: the overall deadline expires, not the test's patience.
    let (_writer, reader) = tokio::io::duplex(8);
    let mut body = RawBody::from_parts(reader, false, None, Vec::new());
    let deadline = tokio::time::Instant::now() + Duration::from_millis(20);
    let err = read_body_with_deadline(&mut body, deadline)
        .await
        .expect_err("deadline must fire");
    assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
    assert!(err.to_string().contains("timed out"), "{err}");

    // Flowing: a body that delivers its bytes still reads to end whole.
    let (mut writer, reader) = tokio::io::duplex(64);
    let mut body = RawBody::from_parts(reader, false, None, Vec::new());
    tokio::spawn(async move {
        use tokio::io::AsyncWriteExt as _;
        let _ = writer.write_all(b"manifest-bytes").await;
        let _ = writer.shutdown().await;
    });
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    assert_eq!(
        read_body_with_deadline(&mut body, deadline).await.unwrap(),
        b"manifest-bytes".to_vec()
    );
}

#[test]
fn body_read_budgets_are_declared() {
    // C2: per-read and overall budgets are named constants, not magic
    // numbers buried at the call sites.
    assert_eq!(BODY_READ_TIMEOUT, Duration::from_secs(30));
    assert_eq!(READ_TIMEOUT, Duration::from_secs(10));
}
