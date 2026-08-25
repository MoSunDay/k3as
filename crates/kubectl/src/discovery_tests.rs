//! Unit tests for the pure `api-versions` / `api-resources` helpers
//! (T7.3 optional enhancement, **Q30**) — no server, no I/O.
//!
//! Fixtures mirror the live wire shapes pinned by
//! `golden/discovery-apis.json` and `golden/discovery-argoproj-v1alpha1.json`.

use serde_json::json;

use crate::discovery::{
    collect_group_versions, render_api_resources, render_api_versions, resource_rows,
    resources_path, sorted_rows,
};

/// Core `GET /api` document (`APIVersions`).
fn core_api() -> serde_json::Value {
    json!({"kind": "APIVersions", "versions": ["v1"]})
}

/// `GET /apis` document (`APIGroupList`) — same shape as
/// golden/discovery-apis.json, trimmed to two groups.
fn apis_groups() -> serde_json::Value {
    json!({
        "kind": "APIGroupList",
        "groups": [
            {"name": "apps",
             "versions": [{"groupVersion": "apps/v1", "version": "v1"}],
             "preferredVersion": {"groupVersion": "apps/v1", "version": "v1"}},
            {"name": "argoproj.io",
             "versions": [{"groupVersion": "argoproj.io/v1alpha1", "version": "v1alpha1"}],
             "preferredVersion": {"groupVersion": "argoproj.io/v1alpha1", "version": "v1alpha1"}},
        ]
    })
}

/// `GET /apis/argoproj.io/v1alpha1` document, byte-shape of the G27 golden.
fn argoproj_list() -> serde_json::Value {
    json!({
        "kind": "APIResourceList",
        "groupVersion": "argoproj.io/v1alpha1",
        "resources": [
            {"name": "cronworkflows", "singularName": "cronWorkflow", "namespaced": true,
             "kind": "CronWorkflow", "shortNames": ["cwf", "cronwf"], "verbs": ["get", "list"]},
            {"name": "workflows", "singularName": "workflow", "namespaced": true,
             "kind": "Workflow", "shortNames": ["wf"], "verbs": ["get", "list"]},
        ]
    })
}

#[test]
fn group_versions_sorted_and_include_core_v1() {
    let versions = collect_group_versions(&core_api(), &apis_groups());
    assert_eq!(
        versions,
        vec![
            "apps/v1".to_string(),
            "argoproj.io/v1alpha1".to_string(),
            "v1".to_string(),
        ]
    );
    // Rendering: one group/version per line, nothing else.
    assert_eq!(
        render_api_versions(&versions),
        "apps/v1\nargoproj.io/v1alpha1\nv1\n"
    );
}

#[test]
fn preferred_version_wins_over_first_listed() {
    let groups = json!({
        "groups": [{
            "name": "example.io",
            "versions": [
                {"groupVersion": "example.io/v2", "version": "v2"},
                {"groupVersion": "example.io/v1", "version": "v1"},
            ],
            "preferredVersion": {"groupVersion": "example.io/v2", "version": "v2"},
        }]
    });
    assert_eq!(
        collect_group_versions(&core_api(), &groups),
        vec!["example.io/v2".to_string(), "v1".to_string()]
    );
}

#[test]
fn resources_path_routes_core_vs_grouped() {
    assert_eq!(resources_path("v1"), "/api/v1");
    assert_eq!(
        resources_path("argoproj.io/v1alpha1"),
        "/apis/argoproj.io/v1alpha1"
    );
}

#[test]
fn resource_rows_map_fields_and_join_shortnames() {
    let rows = resource_rows(&argoproj_list(), "argoproj.io/v1alpha1");
    assert_eq!(rows.len(), 2);
    // Document order is cronworkflows-first; fields map 1:1.
    assert_eq!(
        rows[0],
        crate::discovery::ResourceRow {
            name: "cronworkflows".into(),
            short_names: "cwf,cronwf".into(),
            api_version: "argoproj.io/v1alpha1".into(),
            namespaced: true,
            kind: "CronWorkflow".into(),
        }
    );
    assert_eq!(rows[1].short_names, "wf");
    assert!(rows[1].namespaced);
    assert_eq!(rows[1].kind, "Workflow");
}

#[test]
fn resource_rows_skip_subresources_and_default_fields() {
    let list = json!({
        "resources": [
            {"name": "pods", "namespaced": true, "kind": "Pod"},
            {"name": "pods/status", "namespaced": true, "kind": "Pod"},
            {"name": "pods/exec", "namespaced": true, "kind": "PodAttachOptions"},
            {"name": "namespaces", "namespaced": false, "kind": "Namespace",
             "shortNames": ["ns"]},
        ]
    });
    let rows = resource_rows(&list, "v1");
    assert_eq!(
        rows.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
        vec!["pods", "namespaces"]
    );
    // No shortNames key -> empty column, not "null".
    assert_eq!(rows[0].short_names, "");
    assert_eq!(rows[1].short_names, "ns");
    assert!(!rows[1].namespaced);
}

#[test]
fn rows_sort_by_name_across_groups() {
    let mut rows = resource_rows(&argoproj_list(), "argoproj.io/v1alpha1");
    rows.extend(resource_rows(
        &json!({"resources": [
            {"name": "deployments", "namespaced": true, "kind": "Deployment"},
        ]}),
        "apps/v1",
    ));
    let sorted = sorted_rows(&rows);
    assert_eq!(
        sorted.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
        vec!["cronworkflows", "deployments", "workflows"]
    );
    // Input slice untouched (pure).
    assert_eq!(rows[0].name, "cronworkflows");
    assert_eq!(rows[2].name, "deployments");
}

#[test]
fn render_table_header_and_alignment() {
    let rows = sorted_rows(&resource_rows(&argoproj_list(), "argoproj.io/v1alpha1"));
    let out = render_api_resources(&rows);
    let mut lines = out.lines();
    let header: Vec<&str> = lines.next().unwrap().split_whitespace().collect();
    assert_eq!(
        header,
        vec!["NAME", "SHORTNAMES", "APIVERSION", "NAMESPACED", "KIND"]
    );
    // First column pads to the widest name (cronworkflows); rows are
    // NAME-sorted (cronworkflows < workflows), fields land exactly on
    // whitespace-split columns.
    let cwf: Vec<&str> = lines.next().unwrap().split_whitespace().collect();
    assert_eq!(
        cwf,
        vec![
            "cronworkflows",
            "cwf,cronwf",
            "argoproj.io/v1alpha1",
            "true",
            "CronWorkflow"
        ]
    );
    let wf: Vec<&str> = lines.next().unwrap().split_whitespace().collect();
    assert_eq!(
        wf,
        vec![
            "workflows",
            "wf",
            "argoproj.io/v1alpha1",
            "true",
            "Workflow"
        ]
    );
    // NAMESPACED renders the bare bool words and the header keeps the
    // render_table alignment (dynamic widths, 3-space gaps).
    assert_eq!(
        out.lines().next().unwrap(),
        "NAME            SHORTNAMES   APIVERSION             NAMESPACED   KIND"
    );
    assert!(out.ends_with("Workflow\n"));
}

#[test]
fn render_namespaced_false_and_empty_shortnames() {
    let rows = vec![crate::discovery::ResourceRow {
        name: "namespaces".into(),
        short_names: String::new(),
        api_version: "v1".into(),
        namespaced: false,
        kind: "Namespace".into(),
    }];
    let out = render_api_resources(&rows);
    let row: Vec<&str> = out.lines().nth(1).unwrap().split_whitespace().collect();
    assert_eq!(row, vec!["namespaces", "v1", "false", "Namespace"]);
}
