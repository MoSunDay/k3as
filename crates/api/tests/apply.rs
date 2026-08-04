//! T1.2c — Server-Side Apply field-manager unit tests.
//!
//! Tests the pure-functional SSA algorithm in [`api::apply`] without any HTTP
//! layer. Covers: create-via-apply, update-via-apply, conflict detection,
//! force override, field pruning, and fieldsV1 tree extraction.

use api::apply::{apply_object, extract_field_tree, get_managed_fields, set_managed_fields, ApplyOptions, Operation};
use api::patch::PatchStrategy;
use serde_json::{json, Value};

fn opts(manager: &str) -> ApplyOptions {
    ApplyOptions {
        field_manager: manager.to_string(),
        force: false,
        api_version: "v1".to_string(),
        time: None,
    }
}

fn opts_force(manager: &str) -> ApplyOptions {
    ApplyOptions {
        field_manager: manager.to_string(),
        force: true,
        api_version: "v1".to_string(),
        time: None,
    }
}

fn strategy() -> PatchStrategy {
    PatchStrategy::kubernetes_defaults()
}

fn live_with_mf(result: &api::apply::ApplyResult) -> Value {
    let mut v = result.value.clone();
    set_managed_fields(&mut v, result.managed_fields.clone());
    v
}

#[test]
fn create_via_apply_seeds_managed_fields() {
    let desired = json!({
        "apiVersion": "v1", "kind": "ConfigMap",
        "metadata": {"name": "cm1"},
        "data": {"key": "value"}
    });
    let r = apply_object(None, &desired, &opts("mgr-a"), &strategy());

    assert!(r.created);
    assert!(r.conflicts.is_empty());
    assert_eq!(r.value["data"]["key"], "value");
    assert_eq!(r.managed_fields.len(), 1);
    let mf = &r.managed_fields[0];
    assert_eq!(mf.manager, "mgr-a");
    assert_eq!(mf.operation, Operation::Apply);
    assert!(mf.fields_v1["f:data"]["f:key"].is_object());
}

#[test]
fn update_via_apply_same_manager_merges() {
    let desired1 = json!({
        "apiVersion": "v1", "kind": "ConfigMap",
        "metadata": {"name": "cm1"},
        "data": {"a": "1"}
    });
    let r1 = apply_object(None, &desired1, &opts("mgr-a"), &strategy());
    let live = live_with_mf(&r1);

    let desired2 = json!({
        "apiVersion": "v1", "kind": "ConfigMap",
        "metadata": {"name": "cm1"},
        "data": {"a": "1", "b": "2"}
    });
    let r2 = apply_object(Some(&live), &desired2, &opts("mgr-a"), &strategy());

    assert!(!r2.created);
    assert!(r2.conflicts.is_empty());
    assert_eq!(r2.value["data"]["a"], "1");
    assert_eq!(r2.value["data"]["b"], "2");
    assert_eq!(r2.managed_fields.len(), 1);
}

#[test]
fn conflict_when_different_manager_owns_field() {
    let desired_a = json!({
        "apiVersion": "v1", "kind": "ConfigMap",
        "metadata": {"name": "cm1"},
        "data": {"key": "a-value"}
    });
    let r1 = apply_object(None, &desired_a, &opts("mgr-a"), &strategy());
    let live = live_with_mf(&r1);

    let desired_b = json!({
        "apiVersion": "v1", "kind": "ConfigMap",
        "metadata": {"name": "cm1"},
        "data": {"key": "b-value"}
    });
    let r2 = apply_object(Some(&live), &desired_b, &opts("mgr-b"), &strategy());

    assert!(!r2.created);
    assert_eq!(r2.conflicts.len(), 1);
    assert_eq!(r2.conflicts[0].manager, "mgr-a");
    assert_eq!(r2.value["data"]["key"], "a-value");
}

#[test]
fn force_steals_ownership() {
    let desired_a = json!({
        "apiVersion": "v1", "kind": "ConfigMap",
        "metadata": {"name": "cm1"},
        "data": {"key": "a-value"}
    });
    let r1 = apply_object(None, &desired_a, &opts("mgr-a"), &strategy());
    let live = live_with_mf(&r1);

    let desired_b = json!({
        "apiVersion": "v1", "kind": "ConfigMap",
        "metadata": {"name": "cm1"},
        "data": {"key": "b-value"}
    });
    let r2 = apply_object(Some(&live), &desired_b, &opts_force("mgr-b"), &strategy());

    assert!(r2.conflicts.is_empty());
    assert_eq!(r2.value["data"]["key"], "b-value");

    let b = r2.managed_fields.iter()
        .find(|e| e.manager == "mgr-b" && e.operation == Operation::Apply)
        .expect("mgr-b entry");
    assert!(b.fields_v1["f:data"]["f:key"].is_object());

    let a = r2.managed_fields.iter()
        .find(|e| e.manager == "mgr-a");
    if let Some(a) = a {
        assert!(a.fields_v1.get("f:data").and_then(|d| d.get("f:key")).is_none());
    }
}

#[test]
fn apply_prunes_fields_no_longer_desired() {
    let desired1 = json!({
        "apiVersion": "v1", "kind": "ConfigMap",
        "metadata": {"name": "cm1"},
        "data": {"keep": "1", "drop": "2"}
    });
    let r1 = apply_object(None, &desired1, &opts("mgr-a"), &strategy());
    let live = live_with_mf(&r1);

    let desired2 = json!({
        "apiVersion": "v1", "kind": "ConfigMap",
        "metadata": {"name": "cm1"},
        "data": {"keep": "1"}
    });
    let r2 = apply_object(Some(&live), &desired2, &opts("mgr-a"), &strategy());

    assert!(r2.value["data"].get("drop").is_none());
    assert_eq!(r2.value["data"]["keep"], "1");
}

#[test]
fn different_managers_own_different_fields() {
    let desired_a = json!({
        "apiVersion": "v1", "kind": "ConfigMap",
        "metadata": {"name": "cm1"},
        "data": {"key_a": "a"}
    });
    let r1 = apply_object(None, &desired_a, &opts("mgr-a"), &strategy());
    let live = live_with_mf(&r1);

    let desired_b = json!({
        "apiVersion": "v1", "kind": "ConfigMap",
        "metadata": {"name": "cm1"},
        "data": {"key_b": "b"}
    });
    let r2 = apply_object(Some(&live), &desired_b, &opts("mgr-b"), &strategy());

    assert!(r2.conflicts.is_empty());
    assert_eq!(r2.value["data"]["key_a"], "a");
    assert_eq!(r2.value["data"]["key_b"], "b");
    assert_eq!(r2.managed_fields.len(), 2);
}

#[test]
fn extract_field_tree_containers_by_name() {
    let desired = json!({
        "spec": {
            "containers": [
                {"name": "c1", "image": "nginx"},
                {"name": "c2", "image": "redis"}
            ]
        }
    });
    let tree = extract_field_tree(&desired, &strategy());

    let containers = &tree["f:spec"]["f:containers"];
    assert!(containers["k:{\"name\":\"c1\"}"]["f:image"].is_object());
    assert!(containers["k:{\"name\":\"c2\"}"]["f:image"].is_object());
    assert!(containers["k:{\"name\":\"c1\"}"]["."].is_object());
}

#[test]
fn managed_fields_round_trip_through_json() {
    let desired = json!({"data": {"k": "v"}});
    let r = apply_object(None, &desired, &opts("mgr"), &strategy());
    let mut live = r.value.clone();
    set_managed_fields(&mut live, r.managed_fields.clone());

    let mf = get_managed_fields(&live);
    assert_eq!(mf.len(), 1);
    assert_eq!(mf[0].manager, "mgr");
    assert_eq!(mf[0].operation, Operation::Apply);
    assert!(mf[0].fields_v1["f:data"]["f:k"].is_object());
}
