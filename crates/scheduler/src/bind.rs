//! Bind + status writers (T3.2): the storage side-effects of a cycle —
//! `spec.nodeName` on success (the same semantics as the apiserver's
//! `pods/binding` subresource, slice S2) and the `PodScheduled=False,
//! Unschedulable` condition otherwise, both write-if-changed via
//! `semantic_eq` so controllers and scheduler never fight over status
//! (anti-churn; the Sprint-13 quiesce pattern).

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
    let ns = object::namespace(pod).unwrap_or("default");
    let name = object::name(pod).unwrap_or_default();
    let key = Key::new("", "pods", ns, name);
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
    set_condition(&mut current, "True", None, None);
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
    let ns = object::namespace(pod).unwrap_or("default");
    let name = object::name(pod).unwrap_or_default();
    let key = Key::new("", "pods", ns, name);
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
    set_condition(
        &mut current,
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

/// Insert/replace the `PodScheduled` condition, preserving
/// `lastTransitionTime` when the status is unchanged (upstream anti-churn).
fn set_condition(pod: &mut Value, status: &str, reason: Option<&str>, message: Option<&str>) {
    let now = now_rfc3339(now_unix());
    let conditions = pod
        .pointer_mut("/status/conditions")
        .and_then(|c| c.as_array_mut().cloned())
        .unwrap_or_default();
    let mut next: Vec<Value> = Vec::with_capacity(conditions.len() + 1);
    let mut replaced = false;
    for c in conditions {
        if c.get("type").and_then(|t| t.as_str()) == Some("PodScheduled") {
            let unchanged = c.get("status").and_then(|s| s.as_str()) == Some(status);
            let mut merged = json!({
                "type": "PodScheduled",
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
            "type": "PodScheduled",
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
    pod["status"]["conditions"] = Value::Array(next);
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
