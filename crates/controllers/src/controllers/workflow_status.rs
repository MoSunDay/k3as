//! Workflow status/node-tree composition (TODO **T7.3 Scope B**, decision
//! **Q30**).
//!
//! Pure builders for the `status.nodes` entries the reconciler upserts:
//! the root node (Steps/DAG phase aggregator), one StepGroup node per
//! steps-group, and one Pod node per step/task with its phase derived
//! from the mapped pod. Timestamp discipline (anti-oscillation, the
//! pattern shared with Deployment conditions): `startedAt` is the pod's
//! `creationTimestamp`, else the recorded value, else `now` -- and only
//! once the node actually started; `finishedAt` is stamped once on a
//! terminal phase and then frozen, so a quiesced workflow never rewrites
//! its status. JSON-only wire (Q10).

use serde_json::{json, Value};

/// Authoritative pod -> node mapping (stamped at pod creation).
pub(crate) const NODE_NAME_ANNOTATION: &str = "workflows.argoproj.io/node-name";

/// Node phases that end a node's lifecycle (finishedAt is set once).
pub(crate) const TERMINAL_NODE_PHASES: [&str; 4] = ["Succeeded", "Failed", "Error", "Skipped"];

/// Per-node reconciliation inputs: the mapped pod (if any), the previously
/// recorded node status (timestamps/phases are kept verbatim once set),
/// and the reconciliation-time timestamp.
pub(crate) struct NodeView<'a> {
    pub pod: Option<&'a Value>,
    pub recorded: Option<&'a Value>,
    pub now: &'a str,
}

/// Root node (Steps/DAG): the workflow-level phase aggregator.
pub(crate) fn root_node(
    wf_name: &str,
    root_type: &str,
    phase: &str,
    children: Vec<String>,
    view: &NodeView<'_>,
) -> Value {
    let mut node = json!({
        "id": wf_name, "name": wf_name, "type": root_type,
        "phase": phase, "boundaryID": "",
    });
    if !children.is_empty() {
        node["children"] = json!(children);
    }
    set_started(&mut node, view, true);
    set_finished(&mut node, phase, view);
    node
}

/// StepGroup node (`<wf>[<g>]`): one per steps-group.
pub(crate) fn stepgroup_node(
    id: &str,
    phase: &str,
    children: Vec<String>,
    boundary: &str,
    view: &NodeView<'_>,
) -> Value {
    let mut node = json!({
        "id": id, "name": id, "type": "StepGroup",
        "phase": phase, "boundaryID": boundary,
    });
    if !children.is_empty() {
        node["children"] = json!(children);
    }
    set_started(&mut node, view, phase != "Pending");
    set_finished(&mut node, phase, view);
    node
}

/// Step/task node: phase from the mapped pod, templateName, boundary.
pub(crate) fn pod_node(
    id: &str,
    display: &str,
    template_name: &str,
    phase: &str,
    boundary: &str,
    view: &NodeView<'_>,
) -> Value {
    let mut node = json!({
        "id": id, "name": id, "displayName": display, "type": "Pod",
        "templateName": template_name, "phase": phase, "boundaryID": boundary,
    });
    set_started(&mut node, view, true);
    set_finished(&mut node, phase, view);
    if phase == "Failed" {
        // Surface the pod's own failure info when present.
        if let Some(msg) = view
            .pod
            .and_then(|p| p.pointer("/status/reason").and_then(Value::as_str))
            .or_else(|| {
                view.pod
                    .and_then(|p| p.pointer("/status/message").and_then(Value::as_str))
            })
        {
            node["message"] = json!(msg);
        }
    }
    node
}

/// startedAt: the pod's creationTimestamp, else the recorded value, else
/// `now` -- but only once the node has actually started (future groups
/// stay timestamp-free; recorded values are frozen, so no rewrite churn).
fn set_started(node: &mut Value, view: &NodeView<'_>, started: bool) {
    if let Some(t) = view
        .pod
        .and_then(|p| p.pointer("/metadata/creationTimestamp"))
        .filter(|v| !v.is_null())
        .or_else(|| recorded_field(view.recorded, "startedAt"))
    {
        node["startedAt"] = t.clone();
    } else if started {
        node["startedAt"] = json!(view.now);
    }
}

/// finishedAt: set once, on terminal phases, then frozen.
fn set_finished(node: &mut Value, phase: &str, view: &NodeView<'_>) {
    if TERMINAL_NODE_PHASES.contains(&phase) {
        node["finishedAt"] = recorded_field(view.recorded, "finishedAt")
            .cloned()
            .unwrap_or(json!(view.now));
    }
}

/// The node's recorded `status.phase` (kept when its pod vanished).
pub(crate) fn recorded_phase<'a>(
    nodes: &'a serde_json::Map<String, Value>,
    node_name: &str,
) -> Option<&'a str> {
    nodes
        .get(node_name)
        .and_then(|n| n.get("phase"))
        .and_then(Value::as_str)
}

/// A recorded node field, ignoring JSON nulls.
fn recorded_field<'a>(recorded: Option<&'a Value>, field: &str) -> Option<&'a Value> {
    recorded.and_then(|n| n.get(field)).filter(|v| !v.is_null())
}

/// Workflow-level timestamp: keep the recorded value, else `now`.
pub(crate) fn kept_or_now(recorded: Option<&Value>, now: &str) -> Value {
    recorded
        .filter(|v| !v.is_null())
        .cloned()
        .unwrap_or(json!(now))
}

/// StepGroup node id/name: `<wf>[<g>]`.
pub(crate) fn stepgroup_id(wf_name: &str, group: usize) -> String {
    format!("{wf_name}[{group}]")
}

/// Display name: the trailing segment of the node name (the step/task
/// label; the workflow name itself for container entrypoints).
pub(crate) fn display_name(node_name: &str) -> String {
    node_name
        .rsplit_once('.')
        .map(|(_, d)| d)
        .unwrap_or(node_name)
        .to_string()
}

/// The authoritative pod -> node mapping.
pub(crate) fn pod_node_name(pod: &Value) -> Option<&str> {
    pod.pointer("/metadata/annotations")?
        .get(NODE_NAME_ANNOTATION)?
        .as_str()
}
