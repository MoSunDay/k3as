//! Wire-level tests for `kubectl api-versions` / `kubectl api-resources`
//! (T7.3 optional enhancement, **Q30**).
//!
//! Same canned-server pattern as `verbs_wire_tests`, generalized for the
//! discovery verbs' request fan-out: one listener answers `connections`
//! sequential connections by routing on the request path (the client
//! opens one connection per request, `Connection: close`).

use serde_json::json;

use crate::discovery::{api_resources, api_versions};
use crate::http::HttpClient;

fn http_response(status: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

/// Serve `connections` requests, answering each by routing its path
/// through `respond`; every request byte lands in the returned capture.
async fn serve_discovery(
    connections: usize,
    respond: impl Fn(&str) -> String + Send + Sync + 'static,
) -> (u16, std::sync::Arc<std::sync::Mutex<Vec<u8>>>) {
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let captured = Arc::new(Mutex::new(Vec::new()));
    let sink = captured.clone();
    let responder = Arc::new(respond);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        for _ in 0..connections {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let sink = sink.clone();
            let responder = responder.clone();
            tokio::spawn(async move {
                // Parse from a per-connection buffer: the shared capture
                // accumulates ALL requests, so its first header terminator
                // would always belong to connection #1.
                let mut local: Vec<u8> = Vec::new();
                let mut buf = [0u8; 8192];
                loop {
                    let Ok(n) = sock.read(&mut buf).await else {
                        return;
                    };
                    if n == 0 {
                        return;
                    }
                    local.extend_from_slice(&buf[..n]);
                    sink.lock().unwrap().extend_from_slice(&buf[..n]);
                    let Some(p) = local.windows(4).position(|w| w == b"\r\n\r\n") else {
                        continue;
                    };
                    let head = String::from_utf8_lossy(&local[..p]).to_string();
                    let path = head
                        .lines()
                        .next()
                        .and_then(|l| l.split_whitespace().nth(1))
                        .unwrap_or_default()
                        .to_string();
                    let reply = responder(&path);
                    let _ = sock.write_all(reply.as_bytes()).await;
                    let _ = sock.flush().await;
                    return;
                }
            });
        }
    });
    (port, captured)
}

fn client(port: u16) -> HttpClient {
    HttpClient::parse(&format!("http://127.0.0.1:{port}")).unwrap()
}

/// Whitespace-split fields of the `idx`th output line.
fn fields(output: &str, idx: usize) -> Vec<&str> {
    output
        .lines()
        .nth(idx)
        .unwrap()
        .split_whitespace()
        .collect()
}

#[tokio::test]
async fn api_versions_prints_sorted_group_versions() {
    let groups = json!({
        "kind": "APIGroupList",
        "groups": [
            {"name": "argoproj.io",
             "versions": [{"groupVersion": "argoproj.io/v1alpha1", "version": "v1alpha1"}],
             "preferredVersion": {"groupVersion": "argoproj.io/v1alpha1", "version": "v1alpha1"}},
            {"name": "apps",
             "versions": [{"groupVersion": "apps/v1", "version": "v1"}],
             "preferredVersion": {"groupVersion": "apps/v1", "version": "v1"}},
            {"name": "coordination.k8s.io",
             "versions": [{"groupVersion": "coordination.k8s.io/v1", "version": "v1"}],
             "preferredVersion": {"groupVersion": "coordination.k8s.io/v1", "version": "v1"}},
        ]
    });
    let core = json!({"kind": "APIVersions", "versions": ["v1"]});
    let (port, captured) = serve_discovery(2, move |path| match path {
        "/api" => http_response("200 OK", &core.to_string()),
        "/apis" => http_response("200 OK", &groups.to_string()),
        _ => http_response("404 Not Found", ""),
    })
    .await;
    let out = api_versions(&client(port)).await.unwrap();
    assert_eq!(
        out,
        "apps/v1\nargoproj.io/v1alpha1\ncoordination.k8s.io/v1\nv1\n"
    );

    let req = String::from_utf8_lossy(&captured.lock().unwrap()).to_string();
    assert!(req.contains("GET /api "), "{req}");
    assert!(req.contains("GET /apis "), "{req}");
}

#[tokio::test]
async fn api_resources_lists_workflows_and_cronworkflows() {
    let groups = json!({
        "kind": "APIGroupList",
        "groups": [
            {"name": "argoproj.io",
             "versions": [{"groupVersion": "argoproj.io/v1alpha1", "version": "v1alpha1"}],
             "preferredVersion": {"groupVersion": "argoproj.io/v1alpha1", "version": "v1alpha1"}},
            {"name": "apps",
             "versions": [{"groupVersion": "apps/v1", "version": "v1"}],
             "preferredVersion": {"groupVersion": "apps/v1", "version": "v1"}},
            {"name": "gone.io",
             "versions": [{"groupVersion": "gone.io/v1", "version": "v1"}],
             "preferredVersion": {"groupVersion": "gone.io/v1", "version": "v1"}},
        ]
    });
    // /api + /apis + four group/version lists (gone.io answers 404 and
    // must be skipped, not fatal) = 6 sequential connections.
    let (port, captured) = serve_discovery(6, move |path| {
        let body = match path {
            "/api" => json!({"kind": "APIVersions", "versions": ["v1"]}).to_string(),
            "/apis" => groups.to_string(),
            "/api/v1" => json!({
                "kind": "APIResourceList", "groupVersion": "v1",
                "resources": [
                    {"name": "pods", "singularName": "pod", "namespaced": true,
                     "kind": "Pod", "shortNames": ["po"], "verbs": ["get", "list"]},
                    {"name": "namespaces", "singularName": "namespace", "namespaced": false,
                     "kind": "Namespace", "shortNames": ["ns"], "verbs": ["get", "list"]},
                ]
            })
            .to_string(),
            "/apis/apps/v1" => json!({
                "kind": "APIResourceList", "groupVersion": "apps/v1",
                "resources": [
                    {"name": "deployments", "singularName": "deployment", "namespaced": true,
                     "kind": "Deployment", "shortNames": ["deploy"], "verbs": ["get", "list"]},
                ]
            })
            .to_string(),
            "/apis/argoproj.io/v1alpha1" => json!({
                "kind": "APIResourceList", "groupVersion": "argoproj.io/v1alpha1",
                "resources": [
                    {"name": "workflows", "singularName": "workflow", "namespaced": true,
                     "kind": "Workflow", "shortNames": ["wf"], "verbs": ["get", "list"]},
                    {"name": "cronworkflows", "singularName": "cronWorkflow", "namespaced": true,
                     "kind": "CronWorkflow", "shortNames": ["cwf", "cronwf"],
                     "verbs": ["get", "list"]},
                ]
            })
            .to_string(),
            _ => return http_response("404 Not Found", ""),
        };
        http_response("200 OK", &body)
    })
    .await;
    let out = api_resources(&client(port)).await.unwrap();

    // Header + rows NAME-sorted across the combined listing.
    assert_eq!(
        fields(&out, 0),
        vec!["NAME", "SHORTNAMES", "APIVERSION", "NAMESPACED", "KIND"]
    );
    assert_eq!(
        fields(&out, 1),
        vec![
            "cronworkflows",
            "cwf,cronwf",
            "argoproj.io/v1alpha1",
            "true",
            "CronWorkflow"
        ]
    );
    assert_eq!(
        fields(&out, 2),
        vec!["deployments", "deploy", "apps/v1", "true", "Deployment"]
    );
    assert_eq!(
        fields(&out, 3),
        vec!["namespaces", "ns", "v1", "false", "Namespace"]
    );
    assert_eq!(fields(&out, 4), vec!["pods", "po", "v1", "true", "Pod"]);
    assert_eq!(
        fields(&out, 5),
        vec![
            "workflows",
            "wf",
            "argoproj.io/v1alpha1",
            "true",
            "Workflow"
        ]
    );
    assert_eq!(out.lines().count(), 6);

    let req = String::from_utf8_lossy(&captured.lock().unwrap()).to_string();
    assert!(req.contains("GET /api/v1 "), "{req}");
    assert!(req.contains("GET /apis/argoproj.io/v1alpha1 "), "{req}");
}

#[tokio::test]
async fn api_resources_maps_discovery_http_errors() {
    let (port, _) = serve_discovery(2, |path| match path {
        "/api" => http_response("200 OK", r#"{"versions":["v1"]}"#),
        _ => http_response("500 Internal Server Error", r#"{"message":"boom"}"#),
    })
    .await;
    let err = api_resources(&client(port)).await.unwrap_err();
    assert!(err.contains("GET /apis failed: HTTP 500"), "{err}");
}
