//! Unit + canned-server tests for the T7.3 Scope A argoproj.io verbs (Q30).
//!
//! Pure helpers (plural/path/output/age/table/manifest) run with no server;
//! the wire-level cases reuse the rollout_poll_tests pattern: a tiny
//! in-process TCP responder on an ephemeral port — no external processes.

use serde_json::json;

use crate::http::HttpClient;
use crate::verbs::{
    age_between, apply, delete, get, manifest_target, parse_manifest, parse_output, render_table,
    resolve_plural, resource_path, Output,
};

// ---------- pure helpers ----------

#[test]
fn resolve_plural_whitelist_and_aliases() {
    for kind in ["Workflow", "workflows", "wf", "WF"] {
        assert_eq!(resolve_plural(kind).unwrap(), "workflows", "{kind}");
    }
    for kind in ["CronWorkflow", "cronworkflows", "cwf", "cronwf"] {
        assert_eq!(resolve_plural(kind).unwrap(), "cronworkflows", "{kind}");
    }
}

#[test]
fn resolve_plural_rejects_out_of_scope_kinds() {
    for kind in ["pod", "deployment", "workflowtemplates", ""] {
        let err = resolve_plural(kind).unwrap_err();
        assert!(err.contains("argoproj.io/v1alpha1"), "{kind}: {err}");
        assert!(err.contains("workflow"), "{kind}: {err}");
        assert!(err.contains("cronworkflow"), "{kind}: {err}");
    }
}

#[test]
fn resource_path_collection_and_item() {
    assert_eq!(
        resource_path("workflows", "default", None),
        "/apis/argoproj.io/v1alpha1/namespaces/default/workflows"
    );
    assert_eq!(
        resource_path("cronworkflows", "argo", Some("nightly")),
        "/apis/argoproj.io/v1alpha1/namespaces/argo/cronworkflows/nightly"
    );
}

#[test]
fn parse_output_forms() {
    assert_eq!(parse_output("").unwrap(), Output::Table);
    assert_eq!(parse_output("table").unwrap(), Output::Table);
    assert_eq!(parse_output("json").unwrap(), Output::Json);
    assert_eq!(parse_output("yaml").unwrap(), Output::Yaml);
    assert_eq!(parse_output("name").unwrap(), Output::Name);
    let err = parse_output("wide-json").unwrap_err();
    assert!(err.contains("json, yaml, name"), "{err}");
}

#[test]
fn rfc3339_and_age_rendering() {
    let t0 = 1_768_910_400; // 2026-01-20T12:00:00Z
    assert_eq!(age_between(Some("2026-01-20T12:00:00Z"), t0), "0s");
    assert_eq!(age_between(Some("2026-01-20T11:59:55Z"), t0), "5s");
    assert_eq!(age_between(Some("2026-01-20T11:50:00Z"), t0), "10m");
    assert_eq!(age_between(Some("2026-01-20T09:00:00Z"), t0), "3h");
    assert_eq!(age_between(Some("2026-01-18T12:00:00Z"), t0), "2d");
    // Fractional suffix ignored; space separator accepted.
    assert_eq!(age_between(Some("2026-01-20T11:59:50.123Z"), t0), "10s");
    assert_eq!(age_between(Some("2026-01-20 11:59:50Z"), t0), "10s");
    // Future timestamps clamp to 0 instead of going negative.
    assert_eq!(age_between(Some("2026-01-20T12:01:00Z"), t0), "0s");
    // Missing / malformed -> <unknown>.
    assert_eq!(age_between(None, t0), "<unknown>");
    assert_eq!(age_between(Some("not-a-timestamp"), t0), "<unknown>");
    assert_eq!(age_between(Some("2026-13-40T99:99:99Z"), t0), "<unknown>");
    // Leap day sanity against the civil-date algorithm.
    let leap = age_between(Some("2024-02-29T00:00:00Z"), 1_709_164_800);
    assert_eq!(leap, "0s"); // 2024-02-29T00:00:00Z == 1709164800
}

#[test]
fn render_table_aligns_columns() {
    let items = vec![
        json!({
            "metadata": {"name": "hello-world", "creationTimestamp": "2026-01-20T11:59:55Z"},
            "status": {"phase": "Running"},
        }),
        json!({
            "metadata": {"name": "short", "creationTimestamp": "2026-01-20T09:00:00Z"},
            "status": {}, // no phase -> Unknown
        }),
    ];
    let now = 1_768_910_400; // 2026-01-20T12:00:00Z
    let table = render_table(&items, now);
    let lines: Vec<&str> = table.lines().collect();
    assert_eq!(lines[0], "NAME          PHASE     AGE");
    assert_eq!(lines[1], "hello-world   Running   5s");
    assert_eq!(lines[2], "short         Unknown   3h");
    assert!(table.ends_with('\n'));
}

#[test]
fn render_table_marks_missing_fields() {
    let table = render_table(&[json!({"metadata": {"name": "bare"}, "status": null})], 0);
    let lines: Vec<&str> = table.lines().collect();
    assert_eq!(lines[1], "bare   Unknown   <unknown>");
}

// ---------- manifests ----------

const WF_YAML: &str = "apiVersion: argoproj.io/v1alpha1\nkind: Workflow\nmetadata:\n  name: hello-world\n  namespace: argo\nspec:\n  entrypoint: main\n";

#[test]
fn parse_manifest_accepts_yaml_and_json() {
    let obj = parse_manifest(WF_YAML, "wf.yaml").unwrap();
    assert_eq!(obj["kind"], "Workflow");
    let json_obj = parse_manifest(
        r#"{"kind":"CronWorkflow","metadata":{"name":"c"}}"#,
        "c.json",
    )
    .unwrap();
    assert_eq!(json_obj["kind"], "CronWorkflow");
}

#[test]
fn parse_manifest_rejects_multi_doc_and_empty() {
    let multi = format!("{WF_YAML}---\nkind: Workflow\nmetadata:\n  name: second\n");
    let err = parse_manifest(&multi, "multi.yaml").unwrap_err();
    assert!(err.contains("multi-document"), "{err}");
    let empty = parse_manifest("", "empty.yaml").unwrap_err();
    assert!(empty.contains("no YAML document"), "{empty}");
    // A comment-only file parses to Null — rejected as not-a-mapping.
    let null_doc = parse_manifest("# only a comment\n", "c.yaml").unwrap_err();
    assert!(null_doc.contains("must be a YAML mapping"), "{null_doc}");
    let broken = parse_manifest("metadata: [unclosed", "bad.yaml").unwrap_err();
    assert!(broken.contains("invalid YAML"), "{broken}");
}

#[test]
fn manifest_target_extracts_kind_name_namespace() {
    let obj = parse_manifest(WF_YAML, "wf.yaml").unwrap();
    let (kind, name, gen, ns) = manifest_target(&obj).unwrap();
    assert_eq!(
        (
            kind.as_str(),
            name.as_deref(),
            gen.as_deref(),
            ns.as_deref()
        ),
        ("Workflow", Some("hello-world"), None, Some("argo"))
    );
    // generateName-only manifests are allowed (name -> None, POST create).
    let gen_only =
        manifest_target(&json!({"kind": "Workflow", "metadata": {"generateName": "hello-"}}))
            .unwrap();
    assert_eq!(
        (gen_only.1.as_deref(), gen_only.2.as_deref()),
        (None, Some("hello-"))
    );
    // Neither name nor generateName errors; missing kind errors; empty
    // namespace treated as absent.
    let no_name = manifest_target(&json!({"kind": "Workflow", "metadata": {}})).unwrap_err();
    assert!(no_name.contains(".metadata.name"), "{no_name}");
    let no_kind = manifest_target(&json!({"metadata": {"name": "x"}})).unwrap_err();
    assert!(no_kind.contains(".kind"), "{no_kind}");
    let empty_ns =
        manifest_target(&json!({"kind": "Workflow", "metadata": {"name": "x", "namespace": ""}}))
            .unwrap();
    assert_eq!(empty_ns.3, None);
}

// ---------- flag parsing (lib.rs) ----------

fn args(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| s.to_string()).collect()
}

#[test]
fn get_flags_and_positionals() {
    let (flags, pos) = crate::parse_verb_flags(
        &args(&["get", "wf/hello", "-n", "argo", "-o", "name"]),
        true,
        false,
    )
    .unwrap();
    assert_eq!(flags.namespace, "argo");
    assert_eq!(flags.output, Output::Name);
    assert_eq!(flags.server, "http://127.0.0.1:6443");
    assert_eq!(pos, ["wf/hello"]);
    // --flag=value and long forms.
    let (flags, pos) = crate::parse_verb_flags(
        &args(&["get", "--namespace=argo", "--output=json", "cwf"]),
        true,
        false,
    )
    .unwrap();
    assert_eq!(flags.namespace, "argo");
    assert_eq!(flags.output, Output::Json);
    assert_eq!(pos, ["cwf"]);
}

#[test]
fn verb_flags_reject_the_out_of_verb_ones() {
    let err = crate::parse_verb_flags(&args(&["delete", "-f", "x"]), false, false).unwrap_err();
    assert!(err.contains("only supported by \"kubectl apply\""), "{err}");
    let err = crate::parse_verb_flags(&args(&["delete", "-o", "json"]), false, false).unwrap_err();
    assert!(err.contains("only supported by \"kubectl get\""), "{err}");
    let err = crate::parse_verb_flags(&args(&["get", "wf", "--wat"]), true, false).unwrap_err();
    assert!(err.contains("unknown flag"), "{err}");
    let err = crate::parse_verb_flags(&args(&["get", "wf", "--server"]), true, false).unwrap_err();
    assert!(err.contains("needs an argument"), "{err}");
}

#[test]
fn apply_requires_dash_f() {
    let (flags, pos) =
        crate::parse_verb_flags(&args(&["apply", "-f", "wf.yaml"]), false, true).unwrap();
    assert_eq!(flags.file.as_deref(), Some("wf.yaml"));
    assert!(pos.is_empty());
    let (flags, _) = crate::parse_verb_flags(
        &args(&["apply", "--filename=wf.yaml", "extra"]),
        false,
        true,
    )
    .unwrap();
    assert_eq!(flags.file.as_deref(), Some("wf.yaml"));
    // The -f requirement itself is enforced in run_apply; here we only
    // assert the flag never lands for get/delete (previous test).
}

// ---------- canned-server wire tests ----------

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
