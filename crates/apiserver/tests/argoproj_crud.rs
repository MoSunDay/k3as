//! T7.3 Scope A (decision **Q30**): wire-level integration tests for the
//! `argoproj.io/v1alpha1` group — `Workflow` + `CronWorkflow`.
//!
//! Harness mirrors `rest_crud.rs` / `rest_apply.rs` (real axum handlers over
//! a fresh `EmbeddedStorage`, in-process `oneshot`, no socket — ADR Q17);
//! the registry matches the real server wiring (`crates/cli/src/discovery.rs`).
//! The apiserver is schema-less for CRD groups, so the contract under test is
//! *verbatim passthrough*: what the client POSTs is stored and echoed —
//! upstream CRD parity (`subresources: {}`: `.status` is writable through
//! the main resource, including on create). Watch needs a real-TCP harness
//! (the stream never terminates) — see `rest_watch.rs`.

use std::sync::Arc;

use api::SchemaRegistry;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use serde_json::{json, Value};
use storage::{EmbeddedStorage, StorageBackend};
use tower::ServiceExt;

const GV: &str = "/apis/argoproj.io/v1alpha1";

fn served() -> SchemaRegistry {
    let mut reg = SchemaRegistry::with_core_v1();
    api::initpro::register(&mut reg);
    api::argoproj::register(&mut reg);
    reg
}

/// One router over a fresh embedded store (state is Arc-shared, so cloning
/// the router keeps the same store across requests).
fn app() -> axum::Router {
    let store: Arc<dyn StorageBackend> = Arc::new(EmbeddedStorage::new());
    apiserver::api_app(Arc::new(served()), store, "127.0.0.1:6443".to_string())
}

async fn send(
    router: axum::Router,
    method: &str,
    uri: &str,
    body: Option<(&str, Vec<u8>)>,
) -> (StatusCode, Value) {
    let mut b = Request::builder()
        .method(Method::from_bytes(method.as_bytes()).unwrap())
        .uri(uri);
    if let Some((ct, _)) = body {
        b = b.header("content-type", ct);
    }
    let req = match body {
        Some((_, bytes)) => b.body(Body::from(bytes)),
        None => b.body(Body::empty()),
    }
    .unwrap();
    let resp = router.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn json_body(v: &Value) -> Option<(&'static str, Vec<u8>)> {
    Some(("application/json", serde_json::to_vec(v).unwrap()))
}

fn ns_path(ns: &str, plural: &str) -> String {
    format!("{GV}/namespaces/{ns}/{plural}")
}

fn item_path(ns: &str, plural: &str, name: &str) -> String {
    format!("{}/{}", ns_path(ns, plural), name)
}

async fn create(r: axum::Router, ns: &str, plural: &str, body: &Value) -> (StatusCode, Value) {
    send(r, "POST", &ns_path(ns, plural), json_body(body)).await
}

async fn get(r: axum::Router, ns: &str, plural: &str, name: &str) -> (StatusCode, Value) {
    send(r, "GET", &item_path(ns, plural, name), None).await
}

/// PATCH with an arbitrary content-type + JSON body (generic patch / SSA).
async fn patch(r: axum::Router, uri: &str, ct: &str, body: &Value) -> (StatusCode, Value) {
    let bytes = serde_json::to_vec(body).unwrap();
    send(r, "PATCH", uri, Some((ct, bytes))).await
}

/// The two served kinds, each with one representative body.
fn kinds() -> Vec<(&'static str, &'static str, Value)> {
    vec![
        ("workflows", "Workflow", workflow("wf-main")),
        ("cronworkflows", "CronWorkflow", cron_workflow("cwf-main")),
    ]
}

/// Where a kind keeps its template list (Workflow: `spec.templates`,
/// CronWorkflow: `spec.workflowSpec.templates`).
fn templates(body: &Value, plural: &str) -> Value {
    if plural == "workflows" {
        body["spec"]["templates"].clone()
    } else {
        body["spec"]["workflowSpec"]["templates"].clone()
    }
}

/// Representative Argo Workflows v4 `Workflow`: `entrypoint` + a `steps`
/// template delegating to a real-image container template, plus
/// `arguments.parameters` and a controller-written `.status.phase` that must
/// round-trip verbatim.
fn workflow(name: &str) -> Value {
    json!({
        "apiVersion": "argoproj.io/v1alpha1",
        "kind": "Workflow",
        "metadata": { "name": name, "namespace": "default" },
        "spec": {
            "entrypoint": "main",
            "arguments": { "parameters": [{ "name": "message", "value": "hello-argo" }] },
            "templates": [
                { "name": "main", "steps": [[{ "name": "say", "template": "echo" }]] },
                { "name": "echo", "container": {
                    "name": "main", "image": "busybox:1.36",
                    "command": ["echo"], "args": ["{{workflow.parameters.message}}"]
                } }
            ]
        },
        "status": { "phase": "Running" }
    })
}

/// Representative v4 `CronWorkflow`: `workflowSpec` with one container
/// template, `schedules` (the v4 field that replaced v3 `schedule`) and
/// `concurrencyPolicy: Forbid`.
fn cron_workflow(name: &str) -> Value {
    json!({
        "apiVersion": "argoproj.io/v1alpha1",
        "kind": "CronWorkflow",
        "metadata": { "name": name, "namespace": "default" },
        "spec": {
            "schedules": ["*/5 * * * *"],
            "concurrencyPolicy": "Forbid",
            "workflowSpec": {
                "entrypoint": "tick",
                "templates": [{ "name": "tick", "container": {
                    "name": "main", "image": "busybox:1.36", "command": ["date"]
                } }]
            }
        }
    })
}

#[tokio::test]
async fn discovery_serves_argoproj_group_with_short_names() {
    let r = app();

    let (st, body) = send(r.clone(), "GET", "/apis", None).await;
    assert_eq!(st, StatusCode::OK);
    let listed = body["groups"].as_array().expect("groups array");
    assert!(
        listed.iter().any(|g| g["name"] == "argoproj.io"),
        "/apis must list argoproj.io"
    );

    let (st, body) = send(r, "GET", GV, None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body["groupVersion"], "argoproj.io/v1alpha1");
    let resources = body["resources"]
        .as_array()
        .expect("resources array")
        .clone();
    let find = |name: &str| {
        resources
            .iter()
            .find(|res| res["name"] == name)
            .unwrap_or_else(|| panic!("{name} must be served"))
    };
    let wf = find("workflows");
    assert_eq!(wf["kind"], "Workflow");
    assert_eq!(wf["namespaced"], true);
    assert_eq!(wf["shortNames"], json!(["wf"]));
    assert!(wf["verbs"].as_array().unwrap().iter().any(|v| v == "watch"));
    let cwf = find("cronworkflows");
    assert_eq!(cwf["kind"], "CronWorkflow");
    assert_eq!(cwf["namespaced"], true);
    assert_eq!(cwf["shortNames"], json!(["cwf", "cronwf"]));
}

#[tokio::test]
async fn create_get_list_roundtrip_both_kinds() {
    for (plural, kind, body) in kinds() {
        let r = app();
        let name = body["metadata"]["name"].as_str().unwrap();

        // POST -> 201 with spec + status echoed verbatim.
        let (st, out) = create(r.clone(), "default", plural, &body).await;
        assert_eq!(st, StatusCode::CREATED, "{plural} create");
        assert_eq!(out["kind"], kind);
        assert_eq!(out["apiVersion"], "argoproj.io/v1alpha1");
        assert_eq!(out["metadata"]["name"], name);
        assert_eq!(out["metadata"]["namespace"], "default");
        assert!(!out["metadata"]["resourceVersion"]
            .as_str()
            .unwrap_or("")
            .is_empty());
        assert_eq!(
            out["spec"], body["spec"],
            "{plural}: spec must round-trip verbatim"
        );
        if plural == "workflows" {
            // Upstream CRD parity (`subresources: {}`): status writable on create.
            assert_eq!(out["status"]["phase"], "Running");
        }

        // GET single -> the same object.
        let (st, got) = get(r.clone(), "default", plural, name).await;
        assert_eq!(st, StatusCode::OK, "{plural} get");
        assert_eq!(got["kind"], kind);
        assert_eq!(got["spec"], body["spec"]);

        // List: present in `default`, absent from another namespace.
        let (st, list) = send(r.clone(), "GET", &ns_path("default", plural), None).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(list["kind"], format!("{kind}List"));
        let items = list["items"].as_array().unwrap();
        assert_eq!(items.len(), 1, "{plural} default-ns list");
        assert_eq!(items[0]["metadata"]["name"], name);
        let (st, other) = send(r, "GET", &ns_path("kube-system", plural), None).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(
            other["items"].as_array().unwrap().len(),
            0,
            "{plural} ns isolation"
        );
    }
}

#[tokio::test]
async fn replace_adds_template_and_is_visible_on_get() {
    for (plural, _kind, mut body) in kinds() {
        let r = app();
        let name = body["metadata"]["name"].as_str().unwrap().to_string();
        let (_, out) = create(r.clone(), "default", plural, &body).await;
        let rv = out["metadata"]["resourceVersion"]
            .as_str()
            .unwrap()
            .to_string();
        let before = templates(&body, plural).as_array().unwrap().len();

        // PUT a spec with one more template (kubectl-style: rv carried).
        let extra = json!({ "name": "extra", "container": {
            "name": "main", "image": "alpine:3.20", "command": ["true"]
        } });
        let holder = if plural == "workflows" {
            &mut body["spec"]["templates"]
        } else {
            &mut body["spec"]["workflowSpec"]["templates"]
        };
        holder.as_array_mut().unwrap().push(extra);
        body["metadata"]["resourceVersion"] = json!(rv);
        let path = item_path("default", plural, &name);
        let (st, out) = send(r.clone(), "PUT", &path, json_body(&body)).await;
        assert_eq!(st, StatusCode::OK, "{plural} replace");
        assert_ne!(out["metadata"]["resourceVersion"].as_str().unwrap(), rv);

        let (_, got) = get(r, "default", plural, &name).await;
        assert_eq!(
            templates(&got, plural).as_array().unwrap().len(),
            before + 1,
            "{plural} post-PUT"
        );
        assert_eq!(got["metadata"]["namespace"], "default");
    }
}

#[tokio::test]
async fn merge_patch_changes_label_then_delete_404s() {
    for (plural, kind, body) in kinds() {
        let r = app();
        let name = body["metadata"]["name"].as_str().unwrap().to_string();
        let (st, _) = create(r.clone(), "default", plural, &body).await;
        assert_eq!(st, StatusCode::CREATED);

        // RFC 7386 merge patch touching only metadata.labels.
        let p = json!({ "metadata": { "labels": { "argo-test": "patched" } } });
        let path = item_path("default", plural, &name);
        let (st, out) = patch(r.clone(), &path, "application/merge-patch+json", &p).await;
        assert_eq!(st, StatusCode::OK, "{plural} merge patch");
        assert_eq!(out["kind"], kind);
        assert_eq!(out["metadata"]["labels"]["argo-test"], "patched");
        assert_eq!(
            out["spec"], body["spec"],
            "{plural}: patch must not touch spec"
        );

        let (st, out) = send(r.clone(), "DELETE", &path, None).await;
        assert_eq!(st, StatusCode::OK, "{plural} delete");
        assert_eq!(out["kind"], kind);
        let (st, err) = get(r, "default", plural, &name).await;
        assert_eq!(st, StatusCode::NOT_FOUND, "{plural} get after delete");
        assert_eq!(err["reason"], "NotFound");
    }
}

#[tokio::test]
async fn apply_sets_label_and_managed_fields() {
    for (plural, _kind, body) in kinds() {
        let r = app();
        let name = body["metadata"]["name"].as_str().unwrap().to_string();
        let (st, _) = create(r.clone(), "default", plural, &body).await;
        assert_eq!(st, StatusCode::CREATED);

        // SSA on the existing object: label + annotation only (no spec claims).
        let apply = json!({
            "apiVersion": "argoproj.io/v1alpha1",
            "kind": body["kind"],
            "metadata": {
                "name": name, "namespace": "default",
                "labels": { "argo-test": "applied" },
                "annotations": { "argoproj.io/applied": "true" }
            }
        });
        let uri = format!(
            "{}?fieldManager=argo-mgr",
            item_path("default", plural, &name)
        );
        let (st, out) = patch(r.clone(), &uri, "application/apply-patch+yaml", &apply).await;
        assert!(
            st == StatusCode::OK || st == StatusCode::CREATED,
            "{plural} apply, got {st}"
        );
        assert_eq!(out["metadata"]["labels"]["argo-test"], "applied");
        assert_eq!(
            out["metadata"]["annotations"]["argoproj.io/applied"],
            "true"
        );
        let mf = out["metadata"]["managedFields"]
            .as_array()
            .expect("managedFields array");
        assert_eq!(mf[0]["manager"], "argo-mgr");
        assert_eq!(mf[0]["operation"], "Apply");
        assert_eq!(mf[0]["fieldsType"], "FieldsV1");
        assert_eq!(
            out["spec"], body["spec"],
            "{plural}: apply claimed no spec paths"
        );
    }
}

#[tokio::test]
async fn apply_creates_cronworkflow_on_absent() {
    let r = app();
    let body = cron_workflow("cwf-apply-create");
    let path = item_path("default", "cronworkflows", "cwf-apply-create");
    let uri = format!("{path}?fieldManager=argo-create");
    let (st, out) = patch(r, &uri, "application/apply-patch+yaml", &body).await;
    assert_eq!(st, StatusCode::CREATED, "apply create-on-absent -> 201");
    assert_eq!(out["kind"], "CronWorkflow");
    assert_eq!(out["metadata"]["name"], "cwf-apply-create");
    assert_eq!(
        out["spec"], body["spec"],
        "applied spec stored verbatim (v4 schedules included)"
    );
    assert_eq!(
        out["metadata"]["managedFields"][0]["manager"],
        "argo-create"
    );
}

#[tokio::test]
async fn cronworkflow_v4_schedules_stored_verbatim() {
    let r = app();
    let body = cron_workflow("cwf-schedules");
    let (st, out) = create(r.clone(), "default", "cronworkflows", &body).await;
    assert_eq!(st, StatusCode::CREATED);
    // Schema-less storage: `schedules` must not be rewritten (e.g. into the
    // v3 singular `schedule`) nor defaulted.
    assert_eq!(out["spec"]["schedules"], json!(["*/5 * * * *"]));
    assert_eq!(out["spec"]["concurrencyPolicy"], "Forbid");
    assert_eq!(out["spec"]["workflowSpec"]["entrypoint"], "tick");
    assert!(
        out["spec"].get("schedule").is_none(),
        "v3 `schedule` must not be synthesized"
    );

    let (_, got) = get(r, "default", "cronworkflows", "cwf-schedules").await;
    assert_eq!(got["spec"], body["spec"]);
}

#[tokio::test]
async fn create_with_generate_name_gets_server_side_suffix() {
    let r = app();
    let mut body = workflow("unused");
    // Drop the name; POST must mint `<generateName> + 5-char suffix`
    // (upstream utilrand alphabet) and persist it into the stored object.
    body["metadata"]
        .as_object_mut()
        .unwrap()
        .remove("name")
        .unwrap();
    body["metadata"]["generateName"] = json!("hello-world-");
    let (st, out) = create(r.clone(), "default", "workflows", &body).await;
    assert_eq!(st, StatusCode::CREATED);
    let name = out["metadata"]["name"].as_str().unwrap();
    assert!(
        name.starts_with("hello-world-") && name.len() == "hello-world-".len() + 5,
        "generated name {name:?}"
    );
    let suffix: String = name["hello-world-".len()..].to_string();
    assert!(
        suffix
            .chars()
            .all(|c| "bcdfghjklmnpqrstvwxz2456789".contains(c)),
        "suffix must use the utilrand alphabet: {suffix}"
    );
    // The generated object is addressable by its minted name.
    let (st, got) = get(r, "default", "workflows", name).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(got["metadata"]["name"], json!(name));
    assert_eq!(got["metadata"]["generateName"], json!("hello-world-"));

    // Two generateName POSTs mint distinct objects (upstream parity).
    let r2 = app();
    let (_, out2) = create(r2, "default", "workflows", &body).await;
    assert_ne!(
        out2["metadata"]["name"],
        json!(name),
        "suffixes must not collide"
    );

    // Still an error when neither name nor generateName is set.
    let r3 = app();
    let mut bare = workflow("x");
    bare["metadata"]
        .as_object_mut()
        .unwrap()
        .remove("name")
        .unwrap();
    let (st, err) = create(r3, "default", "workflows", &bare).await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(err["message"].as_str().unwrap().contains("metadata.name"));
}
