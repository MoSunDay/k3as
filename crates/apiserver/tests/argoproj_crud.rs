//! T7.3 Scope A (decision **Q30**): CRUD + discovery integration tests for
//! the `argoproj.io/v1alpha1` group (`Workflow` + `CronWorkflow`).
//!
//! The apiserver is schema-less for CRD groups, so the contract under test
//! is *verbatim passthrough*: what the client POSTs is stored and echoed -
//! upstream CRD parity (`subresources: {}`: `.status` is writable through
//! the main resource, including on create). SSA / verbatim-storage /
//! generateName cases live in `argoproj_ssa.rs`, shared fixtures in
//! `argoproj_support/`; watch needs a real-TCP harness (the stream never
//! terminates): see `rest_watch.rs`.

mod argoproj_support;

use axum::http::StatusCode;
use serde_json::{json, Value};

use argoproj_support::*;

/// Where a kind keeps its template list (Workflow: `spec.templates`,
/// CronWorkflow: `spec.workflowSpec.templates`).
fn templates(body: &Value, plural: &str) -> Value {
    if plural == "workflows" {
        body["spec"]["templates"].clone()
    } else {
        body["spec"]["workflowSpec"]["templates"].clone()
    }
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
