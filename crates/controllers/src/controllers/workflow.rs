//! Argo Workflow controller (TODO **T7.3 Scope B**, decision **Q30**).
//!
//! Reconciles `argoproj.io/v1alpha1` Workflow objects into Pods and writes
//! back `status` with the node tree: root node (Steps/DAG/Pod), one
//! StepGroup node per steps-group, and one Pod node per step/task with
//! phases derived from the owned pods (`status.phase`). Level-driven like
//! the ReplicaSet controller (T3.1a): pods are claimed via the controller
//! ownerReference and the `workflows.argoproj.io/node-name` annotation;
//! groups gate pod creation sequentially (only the active group gets
//! pods), steps within a group are created together (parallel).
//!
//! Documented v1 simplifications:
//! * node IDs are the node NAMES (upstream hashes them) -- deterministic,
//!   human-readable, unique because node names embed the workflow name;
//! * script templates run as `sh -c <source>` and pod-spec defaults are a
//!   flat template-over-spec overlay (see [`workflow_pod`]);
//! * retry/when/withItems/suspend/http/resource/plugin constructs are not
//!   executed: the workflow parks at Pending with a `status.message`
//!   naming the construct.
//!
//! Companion modules: [`workflow_plan`] (entrypoint compilation),
//! [`workflow_phase`] (pure naming/phase math), [`workflow_pod`] (pod
//! assembly) and [`workflow_status`] (node-tree builders).
//!
//! Status writes are write-if-changed (`semantic_eq`, the anti-oscillation
//! pattern shared with ReplicaSet/Deployment): timestamps recorded once
//! are kept verbatim from the existing status, so a quiesced workflow
//! never rewrites. JSON-only wire (Q10); in-process client (Q19).

use std::sync::Arc;

use serde_json::{json, Map, Value};
use storage::{Key, KeyPrefix};

use crate::client::Client;
use crate::controllers::workflow_phase::{node_phase_from_pod, workflow_phase};
use crate::controllers::workflow_plan::{compile, PlannedStep, ROOT_POD, ROOT_STEPS};
use crate::controllers::workflow_pod::build_pod;
use crate::controllers::workflow_status::{
    display_name, kept_or_now, pod_node, pod_node_name, recorded_phase, root_node, stepgroup_id,
    stepgroup_node, NodeView, TERMINAL_NODE_PHASES,
};
use crate::controllers::{is_terminating, owned_by};
use crate::error::ControllerError;
use crate::object::{name, namespace, resource_version, semantic_eq};
use crate::time::now_rfc3339;

const GROUP: &str = "argoproj.io";

/// Reconcile one Workflow: plan -> create the active group's pods ->
/// status (write-if-changed). Terminal workflows are a no-op (upstream
/// keeps the final status forever).
pub async fn reconcile(client: &Arc<dyn Client>, wf: &Value) -> Result<(), ControllerError> {
    let ns = namespace(wf).unwrap_or("default").to_string();
    let Some(wf_name) = name(wf) else {
        return Ok(()); // unparseable object: nothing to converge
    };
    if is_terminating(wf) {
        return Ok(()); // deletionTimestamp set: GC territory (Q20)
    }
    if matches!(
        wf.pointer("/status/phase").and_then(Value::as_str),
        Some("Succeeded") | Some("Failed") | Some("Error")
    ) {
        return Ok(()); // converged
    }
    let uid = wf
        .pointer("/metadata/uid")
        .and_then(Value::as_str)
        .unwrap_or("");
    let now = now_rfc3339(now_secs());

    let plan = match compile(wf) {
        Ok(p) => p,
        Err(reason) => {
            let status = json!({
                "phase": "Pending",
                "message": format!("unsupported template construct: {reason}"),
            });
            return write_status_if_changed(client, wf, &ns, wf_name, status).await;
        }
    };

    // Owned pods, keyed by their node-name annotation.
    let prefix = KeyPrefix::new("", "pods", Some(ns.clone()));
    let pods: Vec<Value> = client
        .list(&prefix)
        .await?
        .into_iter()
        .filter(|p| !is_terminating(p) && owned_by(p, "Workflow", wf_name, uid))
        .collect();
    let existing_nodes = wf
        .pointer("/status/nodes")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();

    let pod_for = |node_name: &str| pods.iter().find(|p| pod_node_name(p) == Some(node_name));
    let step_phase = |s: &PlannedStep| -> String {
        pod_for(&s.node_name)
            .map(node_phase_from_pod)
            .or_else(|| recorded_phase(&existing_nodes, &s.node_name))
            .unwrap_or("Pending")
            .to_string()
    };
    let phases: Vec<Vec<String>> = plan
        .groups
        .iter()
        .map(|g| g.iter().map(step_phase).collect())
        .collect();
    // First group not fully Succeeded is the active one; earlier groups are
    // all-Succeeded by construction, later groups stay podless (gating).
    let active = phases
        .iter()
        .position(|g| g.iter().any(|p| p != "Succeeded"));
    let failed_node = plan
        .groups
        .iter()
        .flatten()
        .zip(phases.iter().flatten())
        .find(|(_, p)| p.as_str() == "Failed" || p.as_str() == "Error")
        .map(|(s, _)| s.node_name.clone());

    // Create the active group's missing pods (all at once: parallel steps).
    // After a failure nothing further is created (v1: no continueOn).
    let mut created = 0usize;
    if failed_node.is_none() {
        if let Some(gi) = active {
            for (i, step) in plan.groups[gi].iter().enumerate() {
                if phases[gi][i] == "Succeeded" || pod_for(&step.node_name).is_some() {
                    continue;
                }
                let pod = build_pod(wf, &ns, uid, step);
                match client
                    .create(&Key::new("", "pods", &ns, &step.pod_name), pod)
                    .await
                {
                    Ok(_) => {
                        created += 1;
                        tracing::debug!(workflow = wf_name, pod = %step.pod_name, "created step pod")
                    }
                    // Deterministic names: a racing duplicate is benign.
                    Err(e) if e.is_already_exists() => {}
                    Err(e) => return Err(e),
                }
            }
        }
    }

    // ---- status composition (cumulative node map) ----
    let flat: Vec<&str> = phases.iter().flatten().map(String::as_str).collect();
    let wf_phase = workflow_phase(&flat, !pods.is_empty() || created > 0);
    let mut nodes = existing_nodes.clone();
    let mut built: Vec<Value> = Vec::new();
    if plan.root_type == ROOT_POD {
        // Container/script entrypoint: the root node IS the pod node.
        let step = &plan.groups[0][0];
        let view = NodeView {
            pod: pod_for(&step.node_name),
            recorded: existing_nodes.get(wf_name),
            now: &now,
        };
        built.push(pod_node(
            wf_name,
            wf_name,
            &step.template_name,
            &phases[0][0],
            "",
            &view,
        ));
    } else {
        let children: Vec<String> = match plan.groups.first() {
            None => vec![],
            // Steps root: every StepGroup node the plan emits (the plan
            // is known up-front, so children cover all groups, reached or
            // not -- a documented v1 simplification).
            Some(_) if plan.root_type == ROOT_STEPS => plan
                .groups
                .iter()
                .enumerate()
                .map(|(g, _)| stepgroup_id(wf_name, g))
                .collect(),
            Some(g) => g.iter().map(|s| s.node_name.clone()).collect(),
        };
        let root_view = NodeView {
            pod: None,
            recorded: existing_nodes.get(wf_name),
            now: &now,
        };
        built.push(root_node(
            wf_name,
            plan.root_type,
            wf_phase,
            children,
            &root_view,
        ));
        for (g, group) in plan.groups.iter().enumerate() {
            let step_children: Vec<String> = group.iter().map(|s| s.node_name.clone()).collect();
            if plan.root_type == ROOT_STEPS {
                let sg_id = stepgroup_id(wf_name, g);
                let started =
                    Some(g) == active || group.iter().any(|s| pod_for(&s.node_name).is_some());
                let phase = if phases[g].iter().all(|p| p == "Succeeded") {
                    "Succeeded"
                } else if started {
                    "Running"
                } else {
                    "Pending"
                };
                let view = NodeView {
                    pod: None,
                    recorded: existing_nodes.get(&sg_id),
                    now: &now,
                };
                built.push(stepgroup_node(&sg_id, phase, step_children, wf_name, &view));
            }
            let boundary = if plan.root_type == ROOT_STEPS {
                stepgroup_id(wf_name, g)
            } else {
                wf_name.to_string()
            };
            for (i, step) in group.iter().enumerate() {
                let view = NodeView {
                    pod: pod_for(&step.node_name),
                    recorded: existing_nodes.get(&step.node_name),
                    now: &now,
                };
                built.push(pod_node(
                    &step.node_name,
                    &display_name(&step.node_name),
                    &step.template_name,
                    &phases[g][i],
                    &boundary,
                    &view,
                ));
            }
        }
    }
    for node in built {
        if let Some(id) = node.get("id").and_then(Value::as_str) {
            nodes.insert(id.to_string(), node);
        }
    }

    let mut status = Map::new();
    status.insert("phase".into(), json!(wf_phase));
    status.insert("nodes".into(), Value::Object(nodes));
    status.insert(
        "startedAt".into(),
        kept_or_now(wf.pointer("/status/startedAt"), &now),
    );
    if let Some(node) = &failed_node {
        status.insert("message".into(), json!(format!("step {node} failed")));
    }
    if TERMINAL_NODE_PHASES.contains(&wf_phase) {
        status.insert(
            "finishedAt".into(),
            kept_or_now(wf.pointer("/status/finishedAt"), &now),
        );
        let mut conditions: Vec<Value> = wf
            .pointer("/status/conditions")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if !conditions
            .iter()
            .any(|c| c.get("type").and_then(Value::as_str) == Some("Completed"))
        {
            conditions.push(json!({"type": "Completed", "status": "True"}));
        }
        status.insert("conditions".into(), Value::Array(conditions));
    }
    write_status_if_changed(client, wf, &ns, wf_name, Value::Object(status)).await
}

/// Write `status` unless semantically unchanged (anti-oscillation).
/// Conflicts propagate: the worker retries (a dropped key would stall a
/// quiesced object forever).
async fn write_status_if_changed(
    client: &Arc<dyn Client>,
    wf: &Value,
    ns: &str,
    wf_name: &str,
    status: Value,
) -> Result<(), ControllerError> {
    let current = wf.get("status").cloned().unwrap_or(json!({}));
    if semantic_eq(&current, &status) {
        return Ok(());
    }
    let mut next = wf.clone();
    next["status"] = status;
    client
        .update(
            &Key::new(GROUP, "workflows", ns, wf_name),
            next,
            resource_version(wf),
        )
        .await?;
    tracing::debug!(workflow = wf_name, "workflow status updated");
    Ok(())
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
