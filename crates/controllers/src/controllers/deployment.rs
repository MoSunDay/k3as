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
use crate::controllers::teardown_hold::{
    evaluate_recreate_hold, hold_message, teardown_watch_from_annotation, teardown_watch_from_pods,
    teardown_watch_to_annotation, RecreateHold, TeardownWatch, TEARDOWN_PENDING_ANNOTATION,
    TEARDOWN_SINCE_ANNOTATION,
};
use crate::controllers::{is_terminating, owned_by};
use crate::error::ControllerError;
use crate::id::{add_label, template_hash};
use crate::object::{name, namespace, owner_reference, resource_version, semantic_eq};
use crate::time::now_rfc3339;

#[cfg(test)]
mod tests;

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

/// INF4 (T7.6, Q62): evaluate + persist the Recreate runtime-teardown
/// hold. Returns the pure verdict, the post-write object (when the
/// Deployment annotations changed; the caller reuses it so the later
/// status write CASes against the fresh resourceVersion), and the hold
/// context the caller's Waiting message needs (S47 review F3): the
/// FRESH hosting-node objects (observed release-ack source) plus the
/// raw teardown-since string.
///
/// All node reads are LIVE (explicit GETs of the distinct hosting nodes
/// for the release acks, one LIST for the gpuLedger sweep) — never cached
/// informer state, so a stale ack or ledger entry cannot slip through on
/// a resync with a quiet event stream. Pods still owned by the old RSs
/// are listed fresh as well; the surviving watch set rides the
/// `init-pro.io/teardown-pending` annotation once the Pod objects are
/// gone. `teardown-since` is written ONCE at Begin and cleared at
/// Complete; `pending` is refreshed only when the union grows.
async fn recreate_hold(
    client: &Arc<dyn Client>,
    dep: &Value,
    ns: &str,
    olds: &[Value],
    now: &str,
) -> Result<(RecreateHold, Option<Value>, Vec<Value>, Option<String>), ControllerError> {
    let dep_name = name(dep).unwrap_or_default();
    let pods = client
        .list(&KeyPrefix::new("", "pods", Some(ns.to_string())))
        .await?;
    let live = teardown_watch_from_pods(olds, &pods);
    let pending = teardown_watch_from_annotation(dep);
    let since = dep
        .pointer("/metadata/annotations")
        .and_then(|a| a.get(TEARDOWN_SINCE_ANNOTATION))
        .and_then(Value::as_str)
        .map(str::to_string);
    if live.is_empty() && pending.is_empty() && since.is_none() {
        // Fast path (INF4): ordinary Pods keep upstream Recreate semantics
        // with zero extra reads — no regression to Recreate/canary tests.
        return Ok((RecreateHold::FastPath, None, Vec::new(), None));
    }
    let all_nodes = client.list(&KeyPrefix::new("", "nodes", None)).await?;
    // Fresh GET per distinct hosting node (the release-ack source).
    let mut hosting: Vec<Value> = Vec::new();
    for w in pending.iter().chain(live.iter()) {
        if !w.node.is_empty() && !hosting.iter().any(|n| name(n) == Some(w.node.as_str())) {
            if let Some(n) = client.get(&Key::new("", "nodes", "", &w.node)).await? {
                hosting.push(n);
            }
        }
    }
    let verdict = evaluate_recreate_hold(&pending, &live, &hosting, &all_nodes, since.as_deref());
    match &verdict {
        RecreateHold::FastPath => Ok((verdict, None, hosting, since)),
        RecreateHold::Begin(watch) => {
            let pending_raw = Value::String(teardown_watch_to_annotation(watch));
            let next = dep_with_annotations(dep, |a| {
                a.insert(TEARDOWN_SINCE_ANNOTATION.into(), json!(now));
                a.insert(TEARDOWN_PENDING_ANNOTATION.into(), pending_raw);
            });
            let written = write_dep(client, dep, ns, dep_name, next).await?;
            if written.is_some() {
                tracing::info!(deployment = dep_name, "recreate teardown hold began");
            }
            Ok((verdict, written, hosting, since))
        }
        RecreateHold::Waiting(_) => {
            // Refresh pending only when the union grew (since stays frozen).
            let mut union = pending.clone();
            for l in &live {
                if !union.iter().any(|w| w.pod_uid == l.pod_uid) {
                    union.push(l.clone());
                }
            }
            if union == pending {
                return Ok((verdict, None, hosting, since));
            }
            union.sort_by(|a, b| a.pod_uid.cmp(&b.pod_uid));
            let pending_raw = Value::String(teardown_watch_to_annotation(&union));
            let next = dep_with_annotations(dep, |a| {
                a.insert(TEARDOWN_PENDING_ANNOTATION.into(), pending_raw);
            });
            Ok((
                verdict,
                write_dep(client, dep, ns, dep_name, next).await?,
                hosting,
                since,
            ))
        }
        RecreateHold::Complete => {
            let next = dep_with_annotations(dep, |a| {
                a.remove(TEARDOWN_SINCE_ANNOTATION);
                a.remove(TEARDOWN_PENDING_ANNOTATION);
            });
            let written = write_dep(client, dep, ns, dep_name, next).await?;
            if written.is_some() {
                tracing::info!(deployment = dep_name, "recreate teardown confirmed");
            }
            Ok((verdict, written, hosting, since))
        }
    }
}

/// CAS-write a mutation of `dep`'s annotations; `None` on a lost CAS race
/// (the verdict still governs the current pass; the 1s resync re-records
/// idempotently), errors otherwise.
async fn write_dep(
    client: &Arc<dyn Client>,
    dep: &Value,
    ns: &str,
    dep_name: &str,
    next: Value,
) -> Result<Option<Value>, ControllerError> {
    let key = Key::new(GROUP, "deployments", ns, dep_name);
    match client.update(&key, next, resource_version(dep)).await {
        Ok(updated) => Ok(Some(updated)),
        Err(e) if e.is_conflict() => Ok(None),
        Err(e) => Err(e),
    }
}

/// `dep` with `metadata.annotations` mutated by `f` (pure clone-then-edit;
/// an absent annotations object is created).
fn dep_with_annotations(dep: &Value, f: impl FnOnce(&mut serde_json::Map<String, Value>)) -> Value {
    let mut next = dep.clone();
    if !next.get("metadata").map(Value::is_object).unwrap_or(false) {
        return next;
    }
    let metadata = next.get_mut("metadata").and_then(Value::as_object_mut);
    if let Some(metadata) = metadata {
        let entry = metadata.entry("annotations").or_insert_with(|| json!({}));
        if let Some(map) = entry.as_object_mut() {
            f(map);
        }
    }
    next
}

/// The Progressing=False message while the hold is active (INF4): the
/// waiting podUIDs/nodes, actionable per-entry.
fn begin_message(watch: &[TeardownWatch]) -> String {
    let entries: Vec<String> = watch
        .iter()
        .map(|w| format!("{}@{}", w.pod_uid, w.node))
        .collect();
    format!(
        "Recreate hold: recording runtime teardown of host-direct/GPU pods: {}",
        entries.join(", ")
    )
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
    // May be replaced by a teardown-hold annotation write below (the
    // status section then CASes against the fresh resourceVersion).
    let mut dep_obj = dep.clone();
    let mut teardown_msg: Option<String> = None;
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
            Strategy::Recreate => {
                let (new_target, old_targets) = recreate_targets(desired, &new_view, &old_views);
                // INF4 (T7.6, Q62): hold the new RS at 0 until the runtime
                // teardown of every old host-direct/GPU pod is confirmed
                // by fresh node status (see rollout's hold docs). Only
                // teardown-relevant pods ever engage this — ordinary
                // Recreate rollouts take the zero-read FastPath.
                let (verdict, written, hold_nodes, hold_since) =
                    recreate_hold(client, dep, ns, &olds, &now_rfc3339(now_secs)).await?;
                if let Some(updated) = written {
                    dep_obj = updated;
                }
                let held = match &verdict {
                    RecreateHold::FastPath | RecreateHold::Complete => None,
                    RecreateHold::Begin(watch) => Some(begin_message(watch)),
                    RecreateHold::Waiting(unconfirmed) => Some(hold_message(
                        unconfirmed,
                        &hold_nodes,
                        hold_since.as_deref(),
                    )),
                };
                if let Some(msg) = held {
                    teardown_msg = Some(msg);
                    (0, old_targets) // new RS pinned at 0 while held
                } else {
                    (new_target, old_targets)
                }
            }
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
    // `dep` may be the annotation-write result above (fresh rv for the
    // status CAS).
    let dep = &dep_obj;
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
    } else if let Some(msg) = &teardown_msg {
        // INF4 (T7.6, Q62): actionable stall while the new RS is held at
        // 0 — the waiting podUIDs/nodes are in the message. Takes priority
        // over ProgressDeadlineExceeded (the specific reason beats the
        // generic one) and over NewReplicaSetAvailable (a held rollout is
        // NOT complete even at desired=0).
        desired_conditions.push(progressing(&now, false, "OldRuntimeTeardownPending", msg));
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
