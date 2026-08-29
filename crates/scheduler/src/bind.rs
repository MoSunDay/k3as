//! Bind + status writers (T3.2): the storage side-effects of a cycle —
//! `spec.nodeName` on success (the same semantics as the apiserver's
//! `pods/binding` subresource, slice S2) and the `PodScheduled=False,
//! Unschedulable` condition otherwise, both write-if-changed via
//! `semantic_eq` so controllers and scheduler never fight over status
//! (anti-churn; the Sprint-13 quiesce pattern).
//!
//! TODO A4: the writers are kind-parameterized — `bind_object` /
//! `mark_unschedulable_object` carry the group/resource/condition-type
//! triple; pods (`""`/`pods`/`PodScheduled`) and `init-pro.io/v1` Agents
//! (`init-pro.io`/`agents`/`Scheduled`) use thin named wrappers.

use std::sync::Arc;

use controllers::time::now_rfc3339;

/// Wall clock for condition timestamps (unix seconds).
fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
use controllers::{object, Client, ControllerError};
use serde_json::{json, Value};
use storage::Key;

/// Write `spec.nodeName` (+ `PodScheduled=True` condition) via CAS. A
/// conflict means someone else bound or mutated the pod first — surface it
/// so the worker can rate-limit-requeue.
pub async fn bind_pod(
    client: &Arc<dyn Client>,
    pod: &Value,
    node: &str,
) -> Result<(), ControllerError> {
    bind_object(client, "", "pods", "PodScheduled", pod, node).await
}

/// Bind an `init-pro.io/v1 Agent` (TODO A4): same CAS semantics, the
/// agent's own key (`/registry/init-pro.io/agents/...`) and the `Scheduled`
/// condition instead of `PodScheduled`.
pub async fn bind_agent(
    client: &Arc<dyn Client>,
    agent: &Value,
    node: &str,
) -> Result<(), ControllerError> {
    bind_object(client, "init-pro.io", "agents", "Scheduled", agent, node).await
}

/// Kind-agnostic bind core (TODO A4): write `spec.nodeName` plus
/// `<condition_type>=True` via CAS on the object's own key. A conflict means
/// someone else bound or mutated the object first — surface it so the worker
/// can rate-limit-requeue.
pub async fn bind_object(
    client: &Arc<dyn Client>,
    group: &str,
    resource: &str,
    condition_type: &str,
    obj: &Value,
    node: &str,
) -> Result<(), ControllerError> {
    let ns = object::namespace(obj).unwrap_or("default");
    let name = object::name(obj).unwrap_or_default();
    let key = Key::new(group, resource, ns, name);
    let fresh = client.get(&key).await?;
    let Some(mut current) = fresh else {
        return Ok(()); // vanished: nothing to bind
    };
    if current
        .pointer("/spec/nodeName")
        .and_then(|v| v.as_str())
        .map(|n| !n.is_empty())
        .unwrap_or(false)
    {
        return Ok(()); // already bound elsewhere: fine
    }
    if let Some(spec) = current.get_mut("spec").and_then(|s| s.as_object_mut()) {
        spec.insert("nodeName".into(), json!(node));
    } else {
        current["spec"] = json!({ "nodeName": node });
    }
    set_typed_condition(&mut current, condition_type, "True", None, None);
    let rev = object::resource_version(&current);
    client.update(&key, current, rev).await.map(|_| ())
}

/// Write `PodScheduled=False, reason=Unschedulable` only when it changes.
/// Returns `true` when a write happened (callers use it for tests + logs).
/// Never errors the worker on NotFound (pod gone = success).
pub async fn mark_unschedulable(
    client: &Arc<dyn Client>,
    pod: &Value,
    reason: &str,
) -> Result<bool, ControllerError> {
    mark_unschedulable_object(client, "", "pods", "PodScheduled", pod, reason).await
}

/// Same as [`mark_unschedulable`] for an `init-pro.io/v1 Agent` (TODO A4):
/// agent key + `Scheduled=False, reason=Unschedulable`, write-if-changed.
pub async fn mark_agent_unschedulable(
    client: &Arc<dyn Client>,
    agent: &Value,
    reason: &str,
) -> Result<bool, ControllerError> {
    mark_unschedulable_object(client, "init-pro.io", "agents", "Scheduled", agent, reason).await
}

/// Kind-agnostic unschedulable writer (TODO A4): `Scheduled`-equivalent
/// condition `False`, reason `Unschedulable`, message
/// `0 nodes are available: {reason}` — only when it changes. Returns `true`
/// when a write happened. Never errors the worker on NotFound (gone = success).
pub async fn mark_unschedulable_object(
    client: &Arc<dyn Client>,
    group: &str,
    resource: &str,
    condition_type: &str,
    obj: &Value,
    reason: &str,
) -> Result<bool, ControllerError> {
    let ns = object::namespace(obj).unwrap_or("default");
    let name = object::name(obj).unwrap_or_default();
    let key = Key::new(group, resource, ns, name);
    let Some(mut current) = client.get(&key).await? else {
        return Ok(false);
    };
    // Skip if bound in the meantime (a node event raced us).
    if current
        .pointer("/spec/nodeName")
        .and_then(|v| v.as_str())
        .map(|n| !n.is_empty())
        .unwrap_or(false)
    {
        return Ok(false);
    }
    let before = current.clone();
    set_typed_condition(
        &mut current,
        condition_type,
        "False",
        Some("Unschedulable"),
        Some(&format!("0 nodes are available: {reason}")),
    );
    if object::semantic_eq(&before, &current) {
        return Ok(false);
    }
    let rev = object::resource_version(&current);
    client.update(&key, current, rev).await.map(|_| true)
}

/// Insert/replace the `PodScheduled` condition (the pod-path spelling).
/// Production goes through [`set_typed_condition`] with the kind's own
/// condition type; this wrapper exists so the unit tests below exercise the
/// exact pod condition shape.
#[cfg(test)]
fn set_condition(pod: &mut Value, status: &str, reason: Option<&str>, message: Option<&str>) {
    set_typed_condition(pod, "PodScheduled", status, reason, message)
}

/// Insert/replace the `condition_type` condition, preserving
/// `lastTransitionTime` when the status is unchanged (upstream anti-churn).
/// `condition_type` is the caller's kind vocabulary: `PodScheduled` for
/// pods, `Scheduled` for `init-pro.io/v1` Agents (TODO A4).
fn set_typed_condition(
    obj: &mut Value,
    condition_type: &str,
    status: &str,
    reason: Option<&str>,
    message: Option<&str>,
) {
    let now = now_rfc3339(now_unix());
    let conditions = obj
        .pointer_mut("/status/conditions")
        .and_then(|c| c.as_array_mut().cloned())
        .unwrap_or_default();
    let mut next: Vec<Value> = Vec::with_capacity(conditions.len() + 1);
    let mut replaced = false;
    for c in conditions {
        if c.get("type").and_then(|t| t.as_str()) == Some(condition_type) {
            let unchanged = c.get("status").and_then(|s| s.as_str()) == Some(status);
            let mut merged = json!({
                "type": condition_type,
                "status": status,
                "lastTransitionTime": if unchanged {
                    c.get("lastTransitionTime").cloned().unwrap_or(json!(now))
                } else {
                    json!(now)
                },
            });
            if let Some(r) = reason {
                merged["reason"] = json!(r);
            }
            if let Some(m) = message {
                merged["message"] = json!(m);
            }
            next.push(merged);
            replaced = true;
        } else {
            next.push(c);
        }
    }
    if !replaced {
        let mut cond = json!({
            "type": condition_type,
            "status": status,
            "lastTransitionTime": now,
        });
        if let Some(r) = reason {
            cond["reason"] = json!(r);
        }
        if let Some(m) = message {
            cond["message"] = json!(m);
        }
        next.push(cond);
    }
    obj["status"]["conditions"] = Value::Array(next);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn set_condition_preserves_transition_time_on_same_status() {
        let mut pod = json!({
            "metadata": {"name": "p", "namespace": "default"},
            "status": {"conditions": [
                {"type": "PodScheduled", "status": "False", "reason": "Unschedulable",
                 "lastTransitionTime": "2020-01-01T00:00:00Z"},
                {"type": "Ready", "status": "True"}
            ]}
        });
        set_condition(&mut pod, "False", Some("Unschedulable"), Some("msg"));
        let conds = pod.pointer("/status/conditions").unwrap();
        assert_eq!(
            conds.pointer("/0/lastTransitionTime"),
            Some(&json!("2020-01-01T00:00:00Z"))
        );
        assert_eq!(conds.pointer("/0/message"), Some(&json!("msg")));
        // Other conditions untouched.
        assert_eq!(conds.pointer("/1/type"), Some(&json!("Ready")));

        set_condition(&mut pod, "True", None, None);
        assert_ne!(
            pod.pointer("/status/conditions/0/lastTransitionTime"),
            Some(&json!("2020-01-01T00:00:00Z"))
        );
        assert!(pod.pointer("/status/conditions/0/reason").is_none());
    }

    #[test]
    fn set_condition_creates_status_when_absent() {
        let mut pod = json!({"metadata": {"name": "p"}});
        set_condition(&mut pod, "False", Some("Unschedulable"), None);
        assert_eq!(
            pod.pointer("/status/conditions/0/type"),
            Some(&json!("PodScheduled"))
        );
        assert_eq!(
            pod.pointer("/status/conditions/0/status"),
            Some(&json!("False"))
        );
    }
}
