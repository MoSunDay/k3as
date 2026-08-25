//! T7.3 Scope A (decision **Q30**): Server-Side Apply, verbatim-storage and
//! generateName integration tests for `argoproj.io/v1alpha1`
//! (`Workflow` + `CronWorkflow`).
//!
//! Split out of `argoproj_crud.rs` (file size limit): SSA on existing
//! objects (label/annotation claims + `managedFields` bookkeeping),
//! apply-create-on-absent, schema-less verbatim storage of the v4
//! `schedules` field, and POST-time `metadata.generateName` suffix minting.
//! Shared fixtures live in `argoproj_support/`; CRUD + discovery cases in
//! `argoproj_crud.rs`.

mod argoproj_support;

use axum::http::StatusCode;
use serde_json::json;

use argoproj_support::*;

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
