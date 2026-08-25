//! Wire-level tests for the T7.3 Scope A argoproj.io verbs (Q30).
//!
//! Same canned-server pattern as rollout_poll_tests: a tiny in-process TCP
//! responder on an ephemeral port captures the real request bytes and
//! answers with a canned response; no external processes.
//!
//! Exercised only via `#[cfg(test)] mod verbs_wire_tests;` in lib.rs.

use serde_json::json;

use crate::http::HttpClient;
use crate::verbs::{apply, delete, get, Output};

const WF_YAML: &str = "apiVersion: argoproj.io/v1alpha1\nkind: Workflow\nmetadata:\n  name: hello-world\n  namespace: argo\nspec:\n  entrypoint: main\n";

/// Accept one connection, capture the whole request (head + body), reply
/// `response`, then close. Returns `(port, captured_request)`.
async fn serve_once(response: String) -> (u16, std::sync::Arc<std::sync::Mutex<Vec<u8>>>) {
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let captured = Arc::new(Mutex::new(Vec::new()));
    let sink = captured.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let Ok((mut sock, _)) = listener.accept().await else {
            return;
        };
        let mut buf = [0u8; 8192];
        while let Ok(n) = sock.read(&mut buf).await {
            if n == 0 {
                break;
            }
            sink.lock().unwrap().extend_from_slice(&buf[..n]);
            let seen = sink.lock().unwrap().clone();
            // Stop once head + Content-Length body (if any) has arrived.
            if let Some(p) = seen.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&seen[..p]).to_ascii_lowercase();
                let body_len: usize = head
                    .lines()
                    .find_map(|l| {
                        l.strip_prefix("content-length:")
                            .and_then(|v| v.trim().parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                if seen.len() >= p + 4 + body_len {
                    break;
                }
            }
        }
        let _ = sock.write_all(response.as_bytes()).await;
        let _ = sock.shutdown().await;
    });
    (port, captured)
}

fn http_response(status: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn client(port: u16) -> HttpClient {
    HttpClient::parse(&format!("http://127.0.0.1:{port}")).unwrap()
}

/// Write `WF_YAML` to a fresh temp file; return its path.
fn temp_manifest(tag: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!("kubectl-verbs-{tag}-{}", std::process::id()));
    std::fs::write(&path, WF_YAML).unwrap();
    path
}

#[tokio::test]
async fn apply_missing_file_errors_before_any_io() {
    let err = apply(&client(1), "default", "definitely-absent.yaml")
        .await
        .unwrap_err();
    assert!(err.contains("cannot read"), "{err}");
}

#[tokio::test]
async fn apply_patches_with_ssa_wire_format() {
    let (port, captured) = serve_once(http_response(
        "201 Created",
        r#"{"kind":"Workflow","metadata":{"name":"hello-world","namespace":"argo"}}"#,
    ))
    .await;
    let manifest = temp_manifest("ssa");
    let out = apply(&client(port), "default", manifest.to_str().unwrap())
        .await
        .unwrap();
    assert_eq!(out, "Workflow/hello-world applied");
    std::fs::remove_file(&manifest).ok();

    let req = String::from_utf8_lossy(&captured.lock().unwrap()).to_string();
    assert!(
        req.starts_with(
            "PATCH /apis/argoproj.io/v1alpha1/namespaces/argo/workflows/hello-world?fieldManager=init-pro-kubectl "
        ),
        "request line: {req}"
    );
    // Namespace precedence: manifest .metadata.namespace ("argo") won over
    // the `-n default` flag value.
    assert!(
        req.contains("Content-Type: application/apply-patch+yaml"),
        "{req}"
    );
    let head = req.split("\r\n\r\n").next().unwrap();
    let cl: usize = head
        .lines()
        .find_map(|l| {
            l.strip_prefix("Content-Length: ")
                .and_then(|v| v.parse().ok())
        })
        .expect("Content-Length header");
    assert_eq!(
        cl,
        req.len() - head.len() - 4,
        "body must be the JSON manifest"
    );
    assert!(req.contains("\"kind\":\"Workflow\""), "{req}");
}

#[tokio::test]
async fn apply_conflict_maps_server_message() {
    let (port, _) = serve_once(http_response(
        "409 Conflict",
        r#"{"reason":"Conflict","message":"Apply failed with 1 conflict: conflict with \"other\""}"#,
    ))
    .await;
    let manifest = temp_manifest("conf");
    let err = apply(&client(port), "default", manifest.to_str().unwrap())
        .await
        .unwrap_err();
    std::fs::remove_file(&manifest).ok();
    assert!(err.contains("conflict with"), "{err}");
}

#[tokio::test]
async fn get_renders_list_table_and_handles_404() {
    let list = json!({
        "kind": "WorkflowList",
        "items": [
            {"metadata": {"name": "hello-world", "creationTimestamp": "2026-01-20T11:59:55Z"},
             "status": {"phase": "Succeeded"}},
        ],
    });
    let (port, _) = serve_once(http_response("200 OK", &list.to_string())).await;
    let out = get(&client(port), "default", "workflows", None, Output::Table)
        .await
        .unwrap();
    let last = out.lines().last().unwrap();
    assert!(last.starts_with("hello-world   Succeeded"), "{out}");

    let (port, _) = serve_once(http_response(
        "404 Not Found",
        r#"{"reason":"NotFound","message":"workflows \"nope\" not found"}"#,
    ))
    .await;
    let err = get(
        &client(port),
        "default",
        "workflows",
        Some("nope"),
        Output::Json,
    )
    .await
    .unwrap_err();
    assert!(err.contains("workflows/nope not found"), "{err}");
}

#[tokio::test]
async fn delete_reports_success_and_404() {
    let (port, captured) = serve_once(http_response(
        "200 OK",
        r#"{"kind":"Status","status":"Success"}"#,
    ))
    .await;
    let out = delete(&client(port), "argo", "cronworkflows", "nightly")
        .await
        .unwrap();
    assert_eq!(out, "cronworkflows/nightly deleted");
    let req = String::from_utf8_lossy(&captured.lock().unwrap()).to_string();
    assert!(
        req.starts_with("DELETE /apis/argoproj.io/v1alpha1/namespaces/argo/cronworkflows/nightly "),
        "{req}"
    );

    let (port, _) = serve_once(http_response("404 Not Found", r#"{"reason":"NotFound"}"#)).await;
    let err = delete(&client(port), "argo", "workflows", "nope")
        .await
        .unwrap_err();
    assert!(err.contains("workflows/nope not found"), "{err}");
}

#[tokio::test]
async fn apply_generate_name_posts_create_to_collection() {
    let (port, captured) = serve_once(http_response(
        "201 Created",
        r#"{"kind":"Workflow","apiVersion":"argoproj.io/v1alpha1","metadata":{"name":"hello-world-x7b2c","namespace":"default"}}"#,
    ))
    .await;
    let path = std::env::temp_dir().join(format!("kubectl-verbs-gen-{}", std::process::id()));
    std::fs::write(
        &path,
        "apiVersion: argoproj.io/v1alpha1\nkind: Workflow\nmetadata:\n  generateName: hello-world-\n",
    )
    .unwrap();
    let out = apply(&client(port), "default", path.to_str().unwrap())
        .await
        .unwrap();
    std::fs::remove_file(&path).ok();
    assert_eq!(out, "Workflow/hello-world-x7b2c created");
    let req = String::from_utf8_lossy(&captured.lock().unwrap()).to_string();
    assert!(
        req.starts_with("POST /apis/argoproj.io/v1alpha1/namespaces/default/workflows "),
        "request line: {req}"
    );
    assert!(req.contains("Content-Type: application/json"), "{req}");
}

#[tokio::test]
async fn get_o_name_prints_qualified_names() {
    let list = json!({
        "kind": "WorkflowList",
        "items": [
            {"apiVersion": "argoproj.io/v1alpha1", "kind": "Workflow",
             "metadata": {"name": "hello-world"}},
            {"apiVersion": "argoproj.io/v1alpha1", "kind": "CronWorkflow",
             "metadata": {"name": "nightly"}},
        ],
    });
    let (port, _) = serve_once(http_response("200 OK", &list.to_string())).await;
    let out = get(&client(port), "default", "workflows", None, Output::Name)
        .await
        .unwrap();
    assert_eq!(
        out,
        "workflow.argoproj.io/hello-world\ncronworkflow.argoproj.io/nightly\n"
    );
}
