//! C7 regression cases for the per-attempt provider broker accept loop
//! (T7.7, Q63): transient accept errors are retried, and a request that
//! never half-closes is closed after the read budget instead of wedging the
//! single accept loop (and so every later provider call of the attempt).

use std::path::PathBuf;
use std::time::Duration;

use nix::libc;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

use super::*;

/// Deny-path config; "http://127.0.0.1:9" is never hit (no allowlisted op).
fn cfg() -> BrokerConfig {
    BrokerConfig {
        upstream_url: "http://127.0.0.1:9".to_string(),
        credential_header: "Authorization".to_string(),
        credential_value: "Bearer canary-SECRET".to_string(),
        allowed_operations: Vec::new(),
    }
}

fn up() -> Upstream {
    parse_upstream("http://127.0.0.1:9").unwrap()
}

/// Bounded read of the single JSON reply (fails the test instead of hanging).
async fn read_reply(stream: &mut UnixStream) -> Value {
    let buf = tokio::time::timeout(Duration::from_secs(5), async {
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).await.unwrap();
        buf
    })
    .await
    .expect("reply within 5s");
    serde_json::from_slice(&buf).unwrap()
}

fn raw(errno: i32) -> std::io::Error {
    std::io::Error::from_raw_os_error(errno)
}

#[test]
fn transient_accept_errors_are_retried() {
    for errno in [
        libc::EAGAIN,
        libc::EINTR,
        libc::ECONNABORTED,
        libc::EMFILE,
        libc::ENFILE,
        libc::ENOMEM,
        libc::EPERM,
        libc::EPROTO,
    ] {
        assert!(
            !accept_error_is_fatal(&raw(errno)),
            "errno {errno} must be transient"
        );
    }
    // Synthetic (no errno) errors of the known-benign kinds retry too.
    assert!(!accept_error_is_fatal(&std::io::Error::new(
        std::io::ErrorKind::Interrupted,
        "eintr"
    )));
    assert!(!accept_error_is_fatal(&std::io::Error::new(
        std::io::ErrorKind::ConnectionAborted,
        "aborted"
    )));
}

#[test]
fn closed_listener_errors_are_fatal() {
    // EBADF/EINVAL/ENOTSOCK: the listener can never accept again.
    for errno in [libc::EBADF, libc::EINVAL, libc::ENOTSOCK] {
        assert!(
            accept_error_is_fatal(&raw(errno)),
            "errno {errno} must be fatal"
        );
    }
    // Unknown errors keep the pre-C7 behavior (end the loop, no hot spin).
    assert!(accept_error_is_fatal(&std::io::Error::other("synthetic")));
    assert!(accept_error_is_fatal(&std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "odd"
    )));
}

#[test]
fn parse_request_shapes() {
    let (op, payload) = parse_request(br#"{"op":"chat.completions","payload":{"m":1}}"#).unwrap();
    assert_eq!(op, "chat.completions");
    assert_eq!(payload, json!({"m": 1}));
    let (_, defaulted) = parse_request(br#"{"op":"x"}"#).unwrap();
    assert_eq!(defaulted, json!({}));
    assert!(parse_request(b"not json").is_none());
    assert!(parse_request(br#"{"op":42}"#).is_none());
}

#[tokio::test]
async fn serve_conn_closes_a_request_that_never_half_closes() {
    let (mut client, server) = UnixStream::pair().unwrap();
    let budget = Duration::from_millis(200);
    let served = tokio::spawn(async move {
        let (c, u) = (cfg(), up());
        serve_conn(server, &c, &u, "agent-x", "att-1", budget).await;
    });
    // A complete request whose writer never closes: pre-C7 this parked the
    // accept loop forever.
    client.write_all(br#"{"op":"evil"}"#).await.unwrap();
    let reply = read_reply(&mut client).await;
    assert_eq!(
        reply,
        json!({ "ok": false, "error": "request read timeout" })
    );
    tokio::time::timeout(Duration::from_secs(5), served)
        .await
        .expect("serve_conn returns")
        .unwrap();
}

#[tokio::test]
async fn serve_conn_still_answers_a_complete_request() {
    let (mut client, server) = UnixStream::pair().unwrap();
    let served = tokio::spawn(async move {
        let (c, u) = (cfg(), up());
        serve_conn(server, &c, &u, "agent-x", "att-2", Duration::from_secs(5)).await;
    });
    client
        .write_all(br#"{"op":"evil","payload":{"m":1}}"#)
        .await
        .unwrap();
    client.shutdown().await.unwrap();
    let reply = read_reply(&mut client).await;
    assert_eq!(reply["ok"], false);
    assert_eq!(reply["error"], "operation not allowlisted: evil");
    served.await.unwrap();
}

#[tokio::test]
async fn stuck_connection_does_not_wedge_later_provider_calls() {
    let dir: PathBuf =
        std::env::temp_dir().join(format!("init-pro-broker-c7-{}", std::process::id()));
    let h = spawn_broker_bounded(cfg(), &dir, "agent-x", "att-3", Duration::from_millis(200))
        .await
        .unwrap();
    // First client: connects, sends a request, never closes.
    let mut stuck = UnixStream::connect(&h.socket_path).await.unwrap();
    stuck.write_all(br#"{"op":"stuck"}"#).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await; // let the loop pick it up
                                                         // Second client: a normal exchange must still get its reply.
    let mut later = UnixStream::connect(&h.socket_path).await.unwrap();
    later.write_all(br#"{"op":"later"}"#).await.unwrap();
    later.shutdown().await.unwrap();
    let reply = read_reply(&mut later).await;
    assert_eq!(
        reply["error"], "operation not allowlisted: later",
        "{reply}"
    );
    // The stuck one is closed with an explicit reason, not parked forever.
    let reply = read_reply(&mut stuck).await;
    assert_eq!(reply["error"], "request read timeout", "{reply}");
    h.revoke().await;
    let _ = std::fs::remove_dir_all(&dir);
}
