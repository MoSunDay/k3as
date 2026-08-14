//! Async tests for [`crate::rollout::rollout_status`] (T3.1b, Q21).
//!
//! Exercises the real poll loop over the real HTTP client against a tiny
//! canned in-process TCP server — no external dependencies, matching the
//! Exercised only via `#[cfg(test)] mod rollout_poll_tests;` in lib.rs.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use crate::http::HttpClient;
use crate::rollout::{rollout_status, Outcome, TIMEOUT_MSG};

/// Serve `next()` (a full HTTP/1.1 response) once per accepted connection
/// on an ephemeral port; return that port.
async fn serve<F>(mut next: F) -> u16
where
    F: FnMut() -> String + Send + 'static,
{
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let response = next();
            tokio::spawn(async move {
                let mut buf = [0u8; 2048];
                // Drain the request head, then answer and close (the client
                // asked for Connection: close).
                loop {
                    match sock.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if buf[..n].windows(4).any(|w| w == b"\r\n\r\n") {
                                break;
                            }
                        }
                    }
                }
                let _ = sock.write_all(response.as_bytes()).await;
            });
        }
    });
    port
}

fn http_response(status: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn deployment_body(name: &str, spec_replicas: u64, status: serde_json::Value) -> String {
    let dep = serde_json::json!({
        "apiVersion": "apps/v1",
        "kind": "Deployment",
        "metadata": {"name": name, "namespace": "default", "generation": 1},
        "spec": {"replicas": spec_replicas},
        "status": status,
    });
    serde_json::to_string(&dep).unwrap()
}

fn client(port: u16) -> HttpClient {
    HttpClient::parse(&format!("http://127.0.0.1:{port}")).unwrap()
}

fn not_found() -> String {
    http_response(
        "404 Not Found",
        "{\"kind\":\"Status\",\"reason\":\"NotFound\",\"message\":\"deployments \\\"missing\\\" not found\"}",
    )
}

fn waiting_dep() -> String {
    http_response(
        "200 OK",
        &deployment_body(
            "r",
            2,
            serde_json::json!({"observedGeneration": 1, "replicas": 2, "updatedReplicas": 1, "availableReplicas": 1}),
        ),
    )
}

fn rolled_out_dep() -> String {
    http_response(
        "200 OK",
        &deployment_body(
            "r",
            2,
            serde_json::json!({"observedGeneration": 1, "replicas": 2, "updatedReplicas": 2, "availableReplicas": 2}),
        ),
    )
}

#[tokio::test]
async fn rollout_status_not_found_is_failure() {
    let port = serve(not_found).await;
    match rollout_status(&client(port), "default", "missing", None).await {
        Outcome::Failure(m) => assert_eq!(
            m,
            "Error from server (NotFound): deployments \"missing\" not found"
        ),
        other => panic!("expected Failure, got {other:?}"),
    }
}

#[tokio::test]
async fn rollout_status_success_on_first_poll() {
    let port = serve(rolled_out_dep).await;
    match rollout_status(&client(port), "default", "r", None).await {
        Outcome::Success(m) => assert_eq!(m, "deployment \"r\" successfully rolled out"),
        other => panic!("expected Success, got {other:?}"),
    }
}

#[tokio::test]
async fn rollout_status_converges_over_polls() {
    let seen = AtomicUsize::new(0);
    let port = serve(move || {
        if seen.fetch_add(1, Ordering::SeqCst) == 0 {
            waiting_dep()
        } else {
            rolled_out_dep()
        }
    })
    .await;
    match rollout_status(&client(port), "default", "r", Some(Duration::from_secs(10))).await {
        Outcome::Success(m) => assert_eq!(m, "deployment \"r\" successfully rolled out"),
        other => panic!("expected Success, got {other:?}"),
    }
}

#[tokio::test]
async fn rollout_status_overall_timeout_is_timeout() {
    let port = serve(waiting_dep).await;
    match rollout_status(
        &client(port),
        "default",
        "r",
        Some(Duration::from_millis(400)),
    )
    .await
    {
        Outcome::Timeout(m) => assert_eq!(m, TIMEOUT_MSG),
        other => panic!("expected Timeout, got {other:?}"),
    }
}

#[tokio::test]
async fn rollout_status_zero_timeout_polls_once() {
    let port = serve(waiting_dep).await;
    match rollout_status(&client(port), "default", "r", Some(Duration::ZERO)).await {
        Outcome::Timeout(m) => assert_eq!(m, TIMEOUT_MSG),
        other => panic!("expected Timeout (single attempt), got {other:?}"),
    }
}

#[tokio::test]
async fn rollout_status_connection_refused_is_failure() {
    // Bind then drop an ephemeral port so nothing is listening on it.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    match rollout_status(&client(port), "default", "r", None).await {
        Outcome::Failure(m) => assert!(m.contains("was refused"), "message: {m}"),
        other => panic!("expected Failure, got {other:?}"),
    }
}
