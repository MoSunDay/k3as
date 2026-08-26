//! Wire-level tests for `kubectl exec` (T4.2 Scope C, Q31): a canned
//! apiserver relay on an ephemeral port captures the real request bytes;
//! no external processes (same pattern as verbs_wire_tests).

use std::sync::{Arc, Mutex};

use serde_json::Value;

use crate::exec_cmd::{exec_path, pod_exec, ExecInvocation};
use crate::http::HttpClient;

fn client(port: u16) -> HttpClient {
    HttpClient::parse(&format!("http://127.0.0.1:{port}")).unwrap()
}

fn invocation(port: u16, ns: &str, name: &str, container: Option<&str>) -> ExecInvocation {
    ExecInvocation {
        server: format!("http://127.0.0.1:{port}"),
        namespace: ns.to_string(),
        name: name.to_string(),
        container: container.map(str::to_string),
        command: vec!["ls".to_string(), "/".to_string()],
    }
}

/// Accept one connection, capture the whole request, answer `response`,
/// then close. Stops reading as soon as head + Content-Length body are in
/// (the client keeps its write side open while awaiting the reply).
async fn serve_once(response: String) -> (u16, Arc<Mutex<Vec<u8>>>) {
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

#[tokio::test]
async fn exec_posts_frozen_contract_and_returns_zero_exit() {
    let (port, captured) = serve_once(http_response(
        "200 OK",
        r#"{"exitCode":0,"output":"bin\netc\n"}"#,
    ))
    .await;
    let http = client(port);
    let inv = invocation(port, "default", "web", None);
    let (rc, out) = pod_exec(&http, &inv).await.unwrap();
    assert_eq!(rc, 0);
    assert_eq!(out, "bin\netc\n");

    let req = String::from_utf8_lossy(&captured.lock().unwrap()).to_string();
    assert!(
        req.starts_with(&format!(
            "POST {} HTTP/1.1",
            exec_path("default", "web", None)
        )),
        "request line: {req}"
    );
    assert!(req.contains("Content-Type: application/json"), "{req}");
    // Command array rides in the JSON body verbatim.
    let body = req.split("\r\n\r\n").nth(1).unwrap();
    let parsed: Value = serde_json::from_str(body).unwrap();
    assert_eq!(
        parsed.get("command").and_then(Value::as_array).map(|a| a
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect::<Vec<_>>()),
        Some(vec!["ls".to_string(), "/".to_string()])
    );
}

#[tokio::test]
async fn exec_container_selector_rides_the_query() {
    let (port, captured) =
        serve_once(http_response("200 OK", r#"{"exitCode":3,"output":"boom"}"#)).await;
    let inv = invocation(port, "kube-system", "web", Some("side"));
    let (rc, _) = pod_exec(&client(port), &inv).await.unwrap();
    assert_eq!(rc, 3, "non-zero remote exit codes surface to the caller");
    let req = String::from_utf8_lossy(&captured.lock().unwrap()).to_string();
    assert!(
        req.starts_with(&format!(
            "POST {} HTTP/1.1",
            exec_path("kube-system", "web", Some("side"))
        )),
        "{req}"
    );
}

#[tokio::test]
async fn exec_non_200_surfaces_the_status_message() {
    let (port, _) = serve_once(http_response(
        "404 Not Found",
        r#"{"kind":"Status","status":"Failure","message":"pods \"web\" not found","reason":"NotFound","code":404}"#,
    ))
    .await;
    let err = pod_exec(&client(port), &invocation(port, "default", "web", None))
        .await
        .unwrap_err();
    assert_eq!(err, "Error from server (NotFound): pods \"web\" not found");
}
