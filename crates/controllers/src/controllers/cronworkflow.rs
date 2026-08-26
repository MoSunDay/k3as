//! Argo CronWorkflow controller (TODO **T7.3 Scope C**, decision **Q30**).
//!
//! Fires child Workflows from `argoproj.io/v1alpha1` CronWorkflow objects
//! on schedule, honoring `spec.concurrencyPolicy` (upstream semantics):
//! **Forbid** skips the tick while a child is active -- and does NOT
//! advance `status.lastScheduledTime` -- **Replace** deletes the active
//! children first, **Allow** (default) always fires. Schedules come from
//! v4 `spec.schedules` (array) or the legacy v3 `spec.schedule` (string)
//! and are evaluated by [`crate::controllers::cron_expr`] (UTC,
//! timezone-naive: `spec.timezone` is not honored in v1). Children are
//! normal Workflows: the Workflow controller (T7.3 Scope B) picks them
//! up from its informer -- no direct handoff -- and the child label
//! `workflows.argoproj.io/cron-workflow` names the parent (upstream
//! convention).
//!
//! The controller reads/writes raw `serde_json::Value` with the wire
//! names of `api::argoproj::cron`: status `active` (ObjectReference
//! list), `lastScheduledTime` (advances ONLY when a child was actually
//! created) and `conditions` (a `SubmissionError` upsert on invalid
//! schedules). Writes are write-if-changed (`semantic_eq`, the
//! anti-oscillation pattern shared with the Workflow controller).
//!
//! Documented v1 simplifications: one child per due tick (however many
//! schedules are due); `successfulJobsHistoryLimit` /
//! `failedJobsHistoryLimit` and `stopStrategy` are not enforced (no
//! history pruning / stop expressions yet).

use std::sync::Arc;

use serde_json::{json, Map, Value};
use storage::{Key, KeyPrefix};

use crate::client::Client;
use crate::controllers::cron_expr::{next_after, parse};
use crate::controllers::{is_terminating, owned_by};
use crate::error::ControllerError;
use crate::object::{name, namespace, owner_reference, resource_version, semantic_eq};
use crate::time::{now_rfc3339, parse_rfc3339};

const GROUP: &str = "argoproj.io";
/// Child-Workflow label naming the owning CronWorkflow (upstream).
pub const CHILD_LABEL: &str = "workflows.argoproj.io/cron-workflow";
/// Workflow phases that no longer count as active children.
const TERMINAL_PHASES: [&str; 3] = ["Succeeded", "Failed", "Error"];

/// Reconcile one CronWorkflow: evaluate the schedule, enforce the
/// concurrency policy, fire a child Workflow when due, maintain status.
pub async fn reconcile(client: &Arc<dyn Client>, cwf: &Value) -> Result<(), ControllerError> {
    let ns = namespace(cwf).unwrap_or("default").to_string();
    let Some(cwf_name) = name(cwf) else {
        return Ok(()); // unparseable object: nothing to converge
    };
    if is_terminating(cwf) {
        return Ok(()); // deletionTimestamp set: GC territory (Q20)
    }
    let uid = cwf
        .pointer("/metadata/uid")
        .and_then(Value::as_str)
        .unwrap_or("");
    let now = now_secs() as i64;

    let schedules = read_schedules(cwf);
    if schedules.is_empty() {
        return Ok(()); // no schedule: no children expected
    }
    if cwf.pointer("/spec/suspend").and_then(Value::as_bool) == Some(true) {
        // Suspended: maintain status (active refresh), never fire.
        let active = active_children(client, &ns, cwf_name, uid).await?;
        let status = build_status(cwf, &active, None, None);
        return write_status_if_changed(client, cwf, &ns, cwf_name, status).await;
    }
    if let Some(bad) = schedules
        .iter()
        .find_map(|s| parse(s).is_none().then_some(s.as_str()))
    {
        // Park the object where the operator can see it (the tick does
        // not fire and lastScheduledTime does not advance).
        let active = active_children(client, &ns, cwf_name, uid).await?;
        let status = build_status(cwf, &active, None, Some(bad));
        return write_status_if_changed(client, cwf, &ns, cwf_name, status).await;
    }

    // Due when any schedule fires in (last, now]. With NO anchor on disk
    // (`lastScheduledTime` never set and no `metadata.creationTimestamp`:
    // this storage does not stamp one) the schedule has never been
    // evaluated -- fire at the first opportunity (a documented v1
    // simplification: one eager initial fire; the anchor persists in
    // `lastScheduledTime` from then on).
    let due = match last_scheduled_or_created(cwf) {
        Some(last) => schedules
            .iter()
            .any(|s| next_after(s, last).is_some_and(|t| t <= now)),
        None => true,
    };
    let mut fired = false;
    if due {
        let active = active_children(client, &ns, cwf_name, uid).await?;
        let policy = cwf
            .pointer("/spec/concurrencyPolicy")
            .and_then(Value::as_str)
            .unwrap_or("Allow");
        if policy != "Forbid" || active.is_empty() {
            if policy == "Replace" {
                for child in &active {
                    delete_child(client, &ns, child).await?;
                }
            }
            fired = fire(client, cwf, &ns, cwf_name, uid, now).await?;
        }
    }

    // Active is recomputed AFTER firing/deleting: the status is the source
    // of truth for the next tick's Forbid/Replace decision.
    let active = active_children(client, &ns, cwf_name, uid).await?;
    let status = build_status(cwf, &active, fired.then(|| now_rfc3339(now as u64)), None);
    write_status_if_changed(client, cwf, &ns, cwf_name, status).await
}

/// v4 `spec.schedules` (array, or the serde-compatible bare string) with
/// the legacy v3 `spec.schedule` fallback; empty when neither is set.
fn read_schedules(cwf: &Value) -> Vec<String> {
    let from_value = |v: &Value| -> Vec<String> {
        match v {
            Value::Array(items) => items
                .iter()
                .filter_map(|s| s.as_str())
                .map(str::to_string)
                .collect(),
            Value::String(s) => vec![s.clone()],
            _ => vec![],
        }
    };
    let v4 = cwf
        .pointer("/spec/schedules")
        .map(from_value)
        .unwrap_or_default();
    if !v4.is_empty() {
        return v4;
    }
    cwf.pointer("/spec/schedule")
        .map(from_value)
        .unwrap_or_default()
}

/// Scheduling anchor: `status.lastScheduledTime`, else
/// `metadata.creationTimestamp` (raw storage writes carry neither: the
/// caller treats a missing anchor as immediately due).
fn last_scheduled_or_created(cwf: &Value) -> Option<i64> {
    ["/status/lastScheduledTime", "/metadata/creationTimestamp"]
        .into_iter()
        .find_map(|p| {
            cwf.pointer(p)
                .and_then(Value::as_str)
                .and_then(parse_rfc3339)
        })
        .map(|t| t as i64)
}

/// Owned, non-terminating, non-terminal child Workflows in `ns`.
async fn active_children(
    client: &Arc<dyn Client>,
    ns: &str,
    cwf_name: &str,
    uid: &str,
) -> Result<Vec<Value>, ControllerError> {
    let prefix = KeyPrefix::new(GROUP, "workflows", Some(ns.to_string()));
    Ok(client
        .list(&prefix)
        .await?
        .into_iter()
        .filter(|wf| {
            !is_terminating(wf) && owned_by(wf, "CronWorkflow", cwf_name, uid) && is_active(wf)
        })
        .collect())
}

/// A missing `status.phase` counts as active (not yet reconciled).
fn is_active(wf: &Value) -> bool {
    !TERMINAL_PHASES.contains(
        &wf.pointer("/status/phase")
            .and_then(Value::as_str)
            .unwrap_or(""),
    )
}

/// Create this tick's child Workflow. Returns false when the
/// deterministic name collided (a same-second duplicate tick: the
/// existing child already covers this fire).
async fn fire(
    client: &Arc<dyn Client>,
    cwf: &Value,
    ns: &str,
    cwf_name: &str,
    uid: &str,
    now: i64,
) -> Result<bool, ControllerError> {
    let child_name = format!("{cwf_name}-{now}");
    let mut labels = Map::new();
    labels.insert(CHILD_LABEL.to_string(), json!(cwf_name));
    // `spec.workflowMetadata.labels` overlay (upstream parity).
    if let Some(extra) = cwf
        .pointer("/spec/workflowMetadata/labels")
        .and_then(Value::as_object)
    {
        for (k, v) in extra {
            labels.insert(k.clone(), v.clone());
        }
    }
    let child = json!({
        "apiVersion": "argoproj.io/v1alpha1",
        "kind": "Workflow",
        "metadata": {
            "name": child_name,
            "namespace": ns,
            "labels": labels,
            "ownerReferences": [owner_reference(
                cwf_name,
                uid,
                "CronWorkflow",
                "argoproj.io/v1alpha1",
            )],
        },
        "spec": cwf.pointer("/spec/workflowSpec").cloned().unwrap_or(json!({})),
    });
    match client
        .create(&Key::new(GROUP, "workflows", ns, &child_name), child)
        .await
    {
        Ok(_) => {
            tracing::debug!(cron_workflow = cwf_name, child = %child_name, "fired child workflow");
            Ok(true)
        }
        Err(e) if e.is_already_exists() => Ok(false),
        Err(e) => Err(e),
    }
}

/// Delete one active child (Replace policy). NotFound is benign (racing
/// GC / duplicate delete).
async fn delete_child(
    client: &Arc<dyn Client>,
    ns: &str,
    child: &Value,
) -> Result<(), ControllerError> {
    let Some(child_name) = name(child) else {
        return Ok(());
    };
    match client
        .delete(&Key::new(GROUP, "workflows", ns, child_name))
        .await
    {
        Ok(()) => Ok(()),
        Err(e) if e.is_not_found() => Ok(()),
        Err(e) => Err(e),
    }
}

/// Compose the status subobject (merged over the existing one so fields
/// owned by other writers survive): `active` ObjectReference list,
/// `lastScheduledTime` (advanced only when `fired_now`), and the
/// SubmissionError condition upsert.
fn build_status(
    cwf: &Value,
    active: &[Value],
    fired_now: Option<String>,
    invalid: Option<&str>,
) -> Value {
    let mut status = cwf
        .pointer("/status")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    status.insert(
        "active".into(),
        Value::Array(active.iter().map(object_reference).collect()),
    );
    status.insert(
        "lastScheduledTime".into(),
        match fired_now {
            Some(t) => json!(t),
            // Keep a recorded time verbatim (anti-oscillation).
            None => cwf
                .pointer("/status/lastScheduledTime")
                .cloned()
                .unwrap_or(Value::Null),
        },
    );
    let mut conditions: Vec<Value> = cwf
        .pointer("/status/conditions")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|c| c.get("type").and_then(Value::as_str) != Some("SubmissionError"))
        .collect();
    if let Some(expr) = invalid {
        conditions.push(json!({
            "type": "SubmissionError",
            "status": "True",
            "message": format!("invalid schedule '{expr}'"),
        }));
    }
    status.insert("conditions".into(), Value::Array(conditions));
    Value::Object(status)
}

/// ObjectReference projection for one child Workflow.
fn object_reference(wf: &Value) -> Value {
    let mut r = json!({
        "apiVersion": "argoproj.io/v1alpha1",
        "kind": "Workflow",
        "name": name(wf).unwrap_or(""),
        "namespace": namespace(wf).unwrap_or("default"),
    });
    if let Some(uid) = wf.pointer("/metadata/uid").and_then(Value::as_str) {
        r["uid"] = json!(uid);
    }
    r
}

/// Write `status` unless semantically unchanged (anti-oscillation).
/// Conflicts propagate: the worker retries (a dropped key would stall a
/// quiesced object forever).
async fn write_status_if_changed(
    client: &Arc<dyn Client>,
    cwf: &Value,
    ns: &str,
    cwf_name: &str,
    status: Value,
) -> Result<(), ControllerError> {
    let current = cwf.get("status").cloned().unwrap_or(json!({}));
    if semantic_eq(&current, &status) {
        return Ok(());
    }
    let mut next = cwf.clone();
    next["status"] = status;
    client
        .update(
            &Key::new(GROUP, "cronworkflows", ns, cwf_name),
            next,
            resource_version(cwf),
        )
        .await?;
    tracing::debug!(cron_workflow = cwf_name, "cronworkflow status updated");
    Ok(())
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
