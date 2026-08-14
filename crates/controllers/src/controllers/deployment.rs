//! Deployment controller (TODO **T3.1b**).
//!
//! Compiles a Deployment into exactly one target ReplicaSet per pod-template
//! hash (`<name>-<hash10>`), drives real rollout semantics -- RollingUpdate
//! pacing (maxSurge rounds up, maxUnavailable down) or Recreate gating via
//! [`super::rollout`] -- drains and deletes superseded ReplicaSets, and
//! writes the aggregate status plus Available/Progressing conditions
//! ([`super::conditions`], the surface `kubectl rollout status` consumes),
//! flipping Progressing to `ProgressDeadlineExceeded` once
//! `spec.progressDeadlineSeconds` (default 600s) passes without progress.
//! Pod-level work is delegated to the ReplicaSet controller (the runner
//! enqueues RS keys on RS events). JSON-only wire (Q10); in-process storage
//! client (Q19).

use std::sync::Arc;

use serde_json::{json, Value};
use storage::{Key, KeyPrefix};

use crate::client::Client;
use crate::controllers::conditions::{
    available, deadline_exceeded, find_condition, merge_conditions, progress_deadline_exceeded,
    progressing,
};
use crate::controllers::rollout::{
    parse_strategy, recreate_targets, resolve_surge, resolve_unavailable, rolling_targets, RsView,
    Strategy,
};
use crate::controllers::{is_terminating, owned_by};
use crate::error::ControllerError;
use crate::id::{add_label, template_hash};
use crate::object::{name, namespace, owner_reference, resource_version, semantic_eq};
use crate::time::now_rfc3339;

const GROUP: &str = "apps";
const DEFAULT_PROGRESS_DEADLINE_SECS: u64 = 600;

/// Owned ReplicaSets of `dep_name` in `ns` (controller ref + kind/name).
async fn owned_replicasets(
    client: &Arc<dyn Client>,
    ns: &str,
    dep_name: &str,
    uid: &str,
) -> Result<Vec<Value>, ControllerError> {
    let prefix = KeyPrefix::new(GROUP, "replicasets", Some(ns.to_string()));
    let all = client.list(&prefix).await?;
    Ok(all
        .into_iter()
        .filter(|rs| owned_by(rs, "Deployment", dep_name, uid))
        .collect())
}

/// Reduce an RS object to the [`RsView`] the rollout math consumes.
fn rs_view(rs: &Value) -> RsView {
    RsView {
        name: name(rs).unwrap_or_default().to_string(),
        spec_replicas: rs
            .pointer("/spec/replicas")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        ready_replicas: rs
            .pointer("/status/readyReplicas")
            .and_then(Value::as_u64)
            .unwrap_or(0),
    }
}

/// Compile the template into the target RS (first sighting); returns the
/// stored object so the same pass can still CAS its replica count.
async fn create_target_rs(
    client: &Arc<dyn Client>,
    dep: &Value,
    target_name: &str,
    short: &str,
    initial_replicas: u64,
) -> Result<Value, ControllerError> {
    let ns = namespace(dep).unwrap_or("default");
    let dep_name = name(dep).unwrap_or_default();
    let template = dep
        .pointer("/spec/template")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let selector = dep
        .pointer("/spec/selector")
        .cloned()
        .unwrap_or_else(|| json!({}));
    // The hash covers the ORIGINAL template; the pod-template-hash label is
    // stamped into the RS template + selector AFTER hashing (upstream order).
    let mut rs_template = template.clone();
    add_label(&mut rs_template, "pod-template-hash", short);
    let mut rs_selector = selector.clone();
    if let Some(ml) = rs_selector
        .get_mut("matchLabels")
        .and_then(Value::as_object_mut)
    {
        ml.insert("pod-template-hash".into(), json!(short));
    }
    let mut rs_labels = template
        .pointer("/metadata/labels")
        .cloned()
        .unwrap_or_else(|| json!({}));
    add_label(&mut rs_labels, "pod-template-hash", short);
    let uid = dep
        .pointer("/metadata/uid")
        .and_then(Value::as_str)
        .unwrap_or("");
    let rs = json!({
        "apiVersion": "apps/v1",
        "kind": "ReplicaSet",
        "metadata": {
            "name": target_name,
            "namespace": ns,
            "labels": rs_labels,
            "ownerReferences": [owner_reference(dep_name, uid, "Deployment", "apps/v1")],
        },
        "spec": {"replicas": initial_replicas, "selector": rs_selector, "template": rs_template},
    });
    match client
        .create(&Key::new(GROUP, "replicasets", ns, target_name), rs)
        .await
    {
        Ok(created) => {
            tracing::debug!(deployment = dep_name, rs = target_name, "created target rs");
            Ok(created)
        }
        // Raced (duplicate delivery): adopt the winner for this pass.
        Err(e) if e.is_already_exists() => Ok(client
            .get(&Key::new(GROUP, "replicasets", ns, target_name))
            .await?
            .unwrap_or_else(|| json!({}))),
        Err(e) => Err(e),
    }
}

/// Reconcile one Deployment: create/scale the target RS per the rollout
/// strategy, drain old RSs, write aggregate status + conditions
/// (write-if-changed both).
pub async fn reconcile(
    client: &Arc<dyn Client>,
    dep: &Value,
    now_secs: u64,
) -> Result<(), ControllerError> {
    let ns = namespace(dep).unwrap_or("default");
    let Some(dep_name) = name(dep) else {
        return Ok(());
    };
    if is_terminating(dep) {
        return Ok(()); // deletionTimestamp set: GC territory (T3.1b)
    }
    let uid = dep
        .pointer("/metadata/uid")
        .and_then(Value::as_str)
        .unwrap_or("");
    let desired = dep
        .pointer("/spec/replicas")
        .and_then(Value::as_u64)
        .unwrap_or(1);
    let template = dep
        .pointer("/spec/template")
        .cloned()
        .unwrap_or(Value::Null);
    if !template.is_object() {
        return Ok(()); // invalid template: nothing to compile
    }
    let paused = dep
        .pointer("/spec/paused")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let deadline = dep
        .pointer("/spec/progressDeadlineSeconds")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_PROGRESS_DEADLINE_SECS);

    let hash = template_hash(&template);
    let short = hash[..10].to_string();
    let target_name = format!("{dep_name}-{short}");

    let owned = owned_replicasets(client, ns, dep_name, uid).await?;
    let target = owned
        .iter()
        .find(|rs| name(rs) == Some(target_name.as_str()))
        .cloned();
    let olds: Vec<Value> = owned
        .into_iter()
        .filter(|rs| name(rs) != Some(target_name.as_str()))
        .collect();

    // Paused: no RS mutations at all; status + conditions below still
    // reflect reality (Progressing is carried through unchanged).
    if !paused {
        // Fresh Deployments (no superseded capacity) start the target RS at
        // `desired`; rollouts start at 0 so the surge cap holds from the
        // first pass.
        let target_now = match target {
            Some(rs) => rs,
            None => {
                let initial = if olds.is_empty() { desired } else { 0 };
                create_target_rs(client, dep, &target_name, &short, initial).await?
            }
        };
        let new_view = rs_view(&target_now);
        let old_views: Vec<RsView> = olds.iter().map(rs_view).collect();
        let spec = dep.get("spec").unwrap_or(&Value::Null);
        let (new_target, old_targets) = match parse_strategy(spec) {
            Strategy::Recreate => recreate_targets(desired, &new_view, &old_views),
            Strategy::RollingUpdate {
                max_surge,
                max_unavailable,
            } => rolling_targets(
                desired,
                resolve_surge(&max_surge, desired),
                resolve_unavailable(&max_unavailable, desired),
                &new_view,
                &old_views,
            ),
        };
        // Scale the target RS to `new_target` (CAS; conflicts propagate so
        // the worker retries via the refreshed cache).
        if new_view.spec_replicas != new_target {
            let mut next = target_now.clone();
            next["spec"]["replicas"] = json!(new_target);
            let key = Key::new(GROUP, "replicasets", ns, &target_name);
            client
                .update(&key, next, resource_version(&target_now))
                .await?;
            tracing::debug!(rs = %target_name, replicas = new_target, "scaled target rs");
        }
        // Drain old RSs to their targets; a fully drained old RS is deleted.
        for rs in &olds {
            let rs_name = name(rs).unwrap_or_default();
            let rs_target = old_targets
                .iter()
                .find(|(n, _)| n == rs_name)
                .map(|(_, t)| *t)
                .unwrap_or(0);
            let current = rs
                .pointer("/spec/replicas")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            if current != rs_target {
                let mut next = rs.clone();
                next["spec"]["replicas"] = json!(rs_target);
                let key = Key::new(GROUP, "replicasets", ns, rs_name);
                client.update(&key, next, resource_version(rs)).await?;
                tracing::debug!(rs = rs_name, replicas = rs_target, "scaled old rs");
            }
            let drained = rs
                .pointer("/status/replicas")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            if drained == 0 {
                client
                    .delete(&Key::new(GROUP, "replicasets", ns, rs_name))
                    .await?;
                tracing::debug!(rs = rs_name, "deleted drained old rs");
            }
        }
    }

    // Aggregate status over the owned RS set (post-mutation read).
    let owned = owned_replicasets(client, ns, dep_name, uid).await?;
    let mut total = 0u64;
    let mut ready = 0u64;
    let mut avail = 0u64;
    let mut updated = 0u64;
    for rs in &owned {
        let r = rs
            .pointer("/status/replicas")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        total += r;
        ready += rs
            .pointer("/status/readyReplicas")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        avail += rs
            .pointer("/status/availableReplicas")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        if name(rs) == Some(target_name.as_str()) {
            updated += r;
        }
    }
    let generation = dep
        .pointer("/metadata/generation")
        .and_then(Value::as_u64)
        .unwrap_or(1);
    let observed = generation;
    let complete = updated == desired && avail == desired && generation <= observed;

    // Conditions (anti-churn merge: converged Deployments never rewrite).
    let now = now_rfc3339(now_secs);
    let existing: Vec<Value> = dep
        .pointer("/status/conditions")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let has_availability = avail >= desired;
    let mut desired_conditions = vec![available(
        &now,
        has_availability,
        if has_availability {
            "MinimumReplicasAvailable"
        } else {
            "MinimumReplicasUnavailable"
        },
        if has_availability {
            "Deployment has minimum availability."
        } else {
            "Deployment does not have minimum availability."
        },
    )];
    if paused {
        // Paused rollouts keep their Progressing condition as-is.
        if let Some(p) = find_condition(&existing, "Progressing") {
            desired_conditions.push(p.clone());
        }
    } else if complete {
        desired_conditions.push(progressing(
            &now,
            true,
            "NewReplicaSetAvailable",
            &format!("Deployment \"{dep_name}\" has successfully progressed."),
        ));
    } else if deadline_exceeded(&existing, now_secs, deadline)
        || progress_deadline_exceeded(&existing)
    {
        // Sticky: once the deadline has been reported, the rollout stays
        // failed until it actually completes (upstream semantics; otherwise
        // every resync tick would flap the condition back).
        desired_conditions.push(progressing(
            &now,
            false,
            "ProgressDeadlineExceeded",
            &format!("Deployment \"{dep_name}\" has exceeded its progress deadline."),
        ));
    } else {
        desired_conditions.push(progressing(
            &now,
            true,
            "ReplicaSetUpdated",
            &format!("ReplicaSet \"{target_name}\" is progressing."),
        ));
    }
    let conditions = merge_conditions(&existing, &desired_conditions, &now);

    let new_status = json!({
        "replicas": total,
        "readyReplicas": ready,
        "availableReplicas": avail,
        "updatedReplicas": updated,
        "observedGeneration": observed,
        "conditions": conditions,
    });
    let current = dep.get("status").cloned().unwrap_or(json!({}));
    if !semantic_eq(&current, &new_status) {
        let mut next = dep.clone();
        next["status"] = new_status;
        let key = Key::new(GROUP, "deployments", ns, dep_name);
        // Conflict propagates: the worker retries against the fresh cache.
        client.update(&key, next, resource_version(dep)).await?;
        tracing::debug!(deployment = dep_name, total, "deployment status updated");
    }
    Ok(())
}
