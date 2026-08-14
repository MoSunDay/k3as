//! ReplicaSet controller (T3.1a).
//!
//! Level-driven pod reconciliation: `owned - desired` creates pods from
//! `spec.template`; surplus pods are deleted preferring unscheduled ones
//! (upstream victim ordering); `status.replicas`/`fullyLabeledReplicas` are
//! written only on change (anti-oscillation). `readyReplicas` is NOT set in
//! v1: pods never report Ready without kubelet (T4.2).

use serde_json::{json, Value};
use storage::{Key, KeyPrefix};

use crate::client::Client;
use crate::controllers::{is_terminating, owned_by};
use crate::error::ControllerError;
use crate::object::{
    name, namespace, owner_reference, resource_version, selector_matches, semantic_eq,
};

/// Reconcile one ReplicaSet toward its desired replica count.
pub async fn reconcile(
    client: &std::sync::Arc<dyn Client>,
    rs: &Value,
) -> Result<(), ControllerError> {
    let ns = namespace(rs).unwrap_or("default");
    let Some(rs_name) = name(rs) else {
        return Ok(()); // unparseable object: nothing to converge
    };
    let uid = rs
        .pointer("/metadata/uid")
        .and_then(Value::as_str)
        .unwrap_or("");
    let desired = rs
        .pointer("/spec/replicas")
        .and_then(Value::as_u64)
        .unwrap_or(1) as i64;
    let selector = rs.pointer("/spec/selector").cloned().unwrap_or(Value::Null);
    let template = rs.pointer("/spec/template").cloned().unwrap_or(Value::Null);

    let prefix = KeyPrefix::new("", "pods", Some(ns.to_string()));
    let owned_pods = |pods: Vec<Value>| -> Vec<Value> {
        pods.into_iter()
            .filter(|p| !is_terminating(p) && owned_by(p, "ReplicaSet", rs_name, uid))
            .collect()
    };

    let owned = owned_pods(client.list(&prefix).await?);
    let diff = desired - owned.len() as i64;

    if diff > 0 {
        for _ in 0..diff {
            let pod_name = format!("{rs_name}-{}", crate::id::rand_suffix(5));
            let mut meta = json!({
                "name": pod_name,
                "namespace": ns,
                "ownerReferences": [owner_reference(rs_name, uid, "ReplicaSet", "apps/v1")],
            });
            if let Some(labels) = template.pointer("/metadata/labels") {
                meta["labels"] = labels.clone();
            }
            let pod = json!({
                "apiVersion": "v1",
                "kind": "Pod",
                "metadata": meta,
                "spec": template.pointer("/spec").cloned().unwrap_or(json!({})),
            });
            match client
                .create(&Key::new("", "pods", ns, pod_name.as_str()), pod)
                .await
            {
                Ok(_) => tracing::debug!(rs = rs_name, pod = %pod_name, "created pod"),
                // Suffix collision: the natural requeue creates the rest.
                Err(e) if e.is_already_exists() => {}
                Err(e) => return Err(e),
            }
        }
    } else if diff < 0 {
        // Prefer deleting unscheduled pods, then break ties by name
        // (deterministic; upstream prefers pods without a nodeName).
        let mut victims = owned.clone();
        victims.sort_by_key(|p| {
            (
                p.pointer("/spec/nodeName").is_some(),
                name(p).unwrap_or_default().to_string(),
            )
        });
        for pod in victims.iter().take((-diff) as usize) {
            let pod_name = name(pod).unwrap_or_default();
            client.delete(&Key::new("", "pods", ns, pod_name)).await?;
            tracing::debug!(rs = rs_name, pod = %pod_name, "deleted surplus pod");
        }
    }

    // Status write-if-changed, computed from the post-mutation state.
    let owned = owned_pods(client.list(&prefix).await?);
    let count = owned.len() as u64;
    let labeled = owned
        .iter()
        .filter(|p| selector_matches(p, &selector))
        .count() as u64;
    let new_status = json!({"replicas": count, "fullyLabeledReplicas": labeled});
    let current = rs.get("status").cloned().unwrap_or(json!({}));
    if !semantic_eq(&current, &new_status) {
        let mut next = rs.clone();
        next["status"] = new_status;
        let key = Key::new("apps", "replicasets", ns, rs_name);
        // Conflicts propagate: a dropped key would stall forever on a
        // quiesced object (nothing re-delivers it); the worker retries.
        client.update(&key, next, resource_version(rs)).await?;
        tracing::debug!(rs = rs_name, replicas = count, "rs status updated");
    }
    Ok(())
}
