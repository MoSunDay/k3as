//! Watch-stream decoder tests over `tokio::io::duplex` (TODO **T4.2**).
//!
//! The apiserver streams watch events as chunked `application/json`, one
//! `{"type","object"}` line per event, so the incremental decoder must cope
//! with: several lines in one chunk, one line split across chunks, the
//! terminal 0-chunk, and plain Content-Length bodies. These tests pin all
//! four behaviors against an in-memory pipe (no sockets, no containerd).

use kubelet::WatchConn;
use tokio::io::AsyncWriteExt;

/// Write `framed` bytes to one duplex half, read lines from the other.
async fn lines_over(framed: &[u8], chunked: bool, content_length: Option<u64>) -> Vec<String> {
    let (mut writer, reader) = tokio::io::duplex(8);
    let mut conn = WatchConn::from_parts(reader, chunked, content_length);
    let payload = framed.to_vec();
    tokio::spawn(async move {
        writer.write_all(&payload).await.unwrap();
        writer.flush().await.unwrap();
        writer.shutdown().await.unwrap();
    });
    let mut out = Vec::new();
    while let Some(l) = conn.next_line().await {
        out.push(l);
        assert!(out.len() < 32, "decoder loop must terminate");
    }
    // Terminal chunk / EOF must make the NEXT call return None too.
    assert!(conn.next_line().await.is_none());
    out
}

#[tokio::test]
async fn one_chunk_with_two_full_lines() {
    let body = b"{\"type\":\"ADDED\",\"n\":1}\n{\"type\":\"MODIFIED\",\"n\":2}\n";
    let mut framed = format!("{:x}\r\n", body.len()).into_bytes();
    framed.extend_from_slice(body);
    framed.extend_from_slice(b"\r\n0\r\n\r\n");
    let lines = lines_over(&framed, true, None).await;
    assert_eq!(
        lines,
        vec![
            "{\"type\":\"ADDED\",\"n\":1}".to_string(),
            "{\"type\":\"MODIFIED\",\"n\":2}".to_string()
        ]
    );
}

#[tokio::test]
async fn line_split_across_chunks() {
    // `{"a":1}\n` arrives as `{"a":` + `1}\n` in two separate chunks; the
    // small duplex buffer (8 bytes) further fragments the reads.
    let mut framed = b"5\r\n{\"a\":".to_vec();
    framed.extend_from_slice(b"\r\n3\r\n1}\n\r\n0\r\n\r\n");
    let lines = lines_over(&framed, true, None).await;
    assert_eq!(lines, vec!["{\"a\":1}".to_string()]);
}

#[tokio::test]
async fn terminal_zero_chunk_ends_stream() {
    // Trailers after the 0-chunk are ignored; so is post-trailer garbage.
    let framed = b"2\r\nhi\r\n0\r\nX-Trailer: y\r\n\r\nSHOULD NEVER SURFACE";
    let lines = lines_over(framed, true, None).await;
    assert_eq!(lines, vec!["hi".to_string()]);
}

#[tokio::test]
async fn chunk_extension_is_ignored() {
    let framed = b"7;ext=1\r\n{\"a\":2}\n\r\n0\r\n\r\n";
    let lines = lines_over(framed, true, None).await;
    assert_eq!(lines, vec!["{\"a\":2}".to_string()]);
}

#[tokio::test]
async fn content_length_mode_stops_at_length() {
    // Bytes beyond Content-Length must never surface (read-to-EOF would).
    let lines = lines_over(b"{\"x\":9}\nIGNORED", false, Some(8)).await;
    assert_eq!(lines, vec!["{\"x\":9}".to_string()]);
}

#[tokio::test]
async fn read_to_eof_mode_emits_trailing_partial_line() {
    let lines = lines_over(b"one\ntwo", false, None).await;
    assert_eq!(lines, vec!["one".to_string(), "two".to_string()]);
}

#[tokio::test]
async fn empty_body_yields_no_lines() {
    assert!(lines_over(b"0\r\n\r\n", true, None).await.is_empty());
    assert!(lines_over(b"", false, Some(0)).await.is_empty());
    assert!(lines_over(b"", false, None).await.is_empty());
}
