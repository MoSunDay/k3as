//! Canary child object shapes (TODO **T8.5**, decision **Q58**): pure
//! clones of the stable Deployment/Service plus the drift-free merge used on
//! every later pass. Re-exported through [`super::plan`] so the controller
//! keeps one decision surface.

use serde_json::{json, Map, Value};

use crate::object::semantic_eq;

use super::super::plan::set_container_image;
use super::plan::{selector_labels, ReleasePlan, PIPELINE_LABEL};

/// Clone the stable Deployment into the canary Deployment: same pod spec,
/// canary selector/template labels, the revision image and the pipeline as
/// controller owner. Status/annotations/cluster metadata are never copied.
pub(super) fn clone_deployment(
    stable: &Value,
    plan: &ReleasePlan,
    labels: &Map<String, Value>,
    owner: Value,
) -> Value {
    let pipeline = pipeline_of(labels);
    let mut child = json!({
        "apiVersion": "apps/v1",
        "kind": "Deployment",
        "metadata": child_metadata(&plan.canary_deployment, stable, owner, pipeline),
        "spec": {
            "replicas": stable.pointer("/spec/replicas").cloned().unwrap_or_else(|| json!(1)),
            "selector": {"matchLabels": selector_labels(pipeline)},
            "template": {
                "metadata": {"labels": Value::Object(labels.clone())},
                "spec": stable.pointer("/spec/template/spec").cloned().unwrap_or_else(|| json!({})),
            },
        },
    });
    if !set_container_image(&mut child, &plan.container, &plan.revision) {
        set_container_image(&mut child, "", &plan.revision); // fallback: first one
    }
    child
}

/// Clone the stable Service into the canary Service: `ClusterIP`, canary
/// selector, the same ports WITHOUT `nodePort` (the entry Service owns the
/// node port), `sessionAffinity` carried over. Cluster IPs, ipFamilies,
/// health-check and load-balancer fields are never copied.
pub(super) fn clone_service(
    stable: &Value,
    plan: &ReleasePlan,
    labels: &Map<String, Value>,
    owner: Value,
) -> Value {
    let pipeline = pipeline_of(labels);
    let ports: Vec<Value> = stable
        .pointer("/spec/ports")
        .and_then(Value::as_array)
        .map(|ports| ports.iter().map(strip_node_port).collect())
        .unwrap_or_default();
    let mut spec = json!({
        "type": "ClusterIP",
        "selector": Value::Object(labels.clone()),
        "ports": ports,
    });
    if let Some(affinity) = stable
        .pointer("/spec/sessionAffinity")
        .filter(|v| !v.is_null())
    {
        spec["sessionAffinity"] = affinity.clone();
    }
    json!({
        "apiVersion": "v1",
        "kind": "Service",
        "metadata": child_metadata(&plan.canary_service, stable, owner, pipeline),
        "spec": spec,
    })
}

/// Overwrite ONLY the fields [`clone_deployment`] owns; `None` = no diff.
pub(super) fn merge_deployment(current: &Value, desired: &Value) -> Option<Value> {
    let mut next = current.clone();
    if !next.is_object() {
        return None;
    }
    for path in ["replicas", "selector", "template/metadata/labels"] {
        if let Some(value) = desired.pointer(&format!("/spec/{path}")) {
            set_path(&mut next, &format!("spec/{path}"), value.clone());
        }
    }
    copy_container_images(&mut next, desired);
    (!semantic_eq(current, &next)).then_some(next)
}

/// Overwrite ONLY the fields [`clone_service`] owns; `None` = no diff.
pub(super) fn merge_service(current: &Value, desired: &Value) -> Option<Value> {
    let mut next = current.clone();
    if !next.is_object() {
        return None;
    }
    for field in ["type", "selector", "ports"] {
        if let Some(value) = desired.pointer(&format!("/spec/{field}")) {
            next["spec"][field] = value.clone();
        }
    }
    (!semantic_eq(current, &next)).then_some(next)
}

/// `metadata` of a canary child (identity labels + the controller ownerRef).
fn child_metadata(name: &str, stable: &Value, owner: Value, pipeline: &str) -> Value {
    json!({
        "name": name,
        "namespace": stable.pointer("/metadata/namespace").cloned().unwrap_or_else(|| json!("")),
        "labels": selector_labels(pipeline),
        "ownerReferences": [owner],
    })
}

/// The pipeline name carried by the canary identity labels.
fn pipeline_of(labels: &Map<String, Value>) -> &str {
    labels
        .get(PIPELINE_LABEL)
        .and_then(Value::as_str)
        .unwrap_or("")
}

/// Assign `value` at the `/`-separated `path` (creating objects on the way).
fn set_path(root: &mut Value, path: &str, value: Value) {
    let mut target = root;
    for segment in path.split('/') {
        target = &mut target[segment];
    }
    *target = value;
}

/// `port` without its `nodePort` key (a ClusterIP child never owns one).
fn strip_node_port(port: &Value) -> Value {
    let mut port = port.clone();
    if let Some(obj) = port.as_object_mut() {
        obj.remove("nodePort");
    }
    port
}

/// Copy the desired container images onto `current` (by name, else the only
/// desired container, else the same index).
fn copy_container_images(current: &mut Value, desired: &Value) {
    let wanted = desired
        .pointer("/spec/template/spec/containers")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let Some(have) = current
        .pointer_mut("/spec/template/spec/containers")
        .and_then(Value::as_array_mut)
    else {
        return;
    };
    for (index, container) in have.iter_mut().enumerate() {
        let name = container.get("name").and_then(Value::as_str).unwrap_or("");
        let source = wanted
            .iter()
            .find(|w| w.get("name").and_then(Value::as_str).unwrap_or("") == name)
            .or_else(|| (wanted.len() == 1).then(|| &wanted[0]))
            .or_else(|| wanted.get(index));
        if let Some(image) = source.and_then(|w| w.get("image")) {
            container["image"] = image.clone();
        }
    }
}
