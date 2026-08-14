//! Deployment controller (T3.1a).
//!
//! Compiles a Deployment into exactly one target ReplicaSet per pod-template
//! hash (`<name>-<hash10>`), scales it to `spec.replicas`, drains and deletes
//! superseded ReplicaSets, and reports aggregate status. Rollout strategy in
//! v1 is instant scale-up + drain-down (Recreate-flavored); maxSurge /
//! maxUnavailable rolling updates are **T3.1b**. Pod-level work is delegated
//! to the ReplicaSet controller (the runner enqueues RS keys on RS events).

use serde_json::{json, Value};
use storage::{Key, KeyPrefix};

use crate::client::Client;
use crate::controllers::owned_by;
use crate::error::ControllerError;
use crate::id::{add_label, template_hash};
use crate::object::{name, namespace, owner_reference, resource_version, semantic_eq};

const GROUP: &str = "apps";

/// Owned ReplicaSets of `dep_name` in `ns` (controller ref + kind/name).
async fn owned_replicasets(
    client: &std::sync::Arc<dyn Client>,
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

/// Reconcile one Deployment: create/scale the target RS, drain old RSs,
/// write aggregate status (write-if-changed).
pub async fn reconcile(
    client: &std::sync::Arc<dyn Client>,
    dep: &Value,
) -> Result<(), ControllerError> {
    let ns = namespace(dep).unwrap_or("default");
    let Some(dep_name) = name(dep) else {
        return Ok(());
    };
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
        return Ok(()); // invalid spec: wait for a fix
    }
    let selector = dep
        .pointer("/spec/selector")
        .cloned()
        .unwrap_or(Value::Null);

    // The hash covers the ORIGINAL template; the pod-template-hash label is
    // stamped into the RS template + selector AFTER hashing (upstream order).
    let hash = template_hash(&template);
    let short = hash[..10].to_string();
    let target_name = format!("{dep_name}-{short}");

    let owned = owned_replicasets(client, ns, dep_name, uid).await?;
    let target = owned
        .iter()
        .find(|rs| name(rs) == Some(target_name.as_str()))
        .cloned();

    if let Some(rs) = &target {
        // Scale the existing target RS to the desired count.
        let current = rs
            .pointer("/spec/replicas")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        if current != desired {
            let mut next = rs.clone();
            next["spec"]["replicas"] = json!(desired);
            let key = Key::new(GROUP, "replicasets", ns, name(rs).unwrap_or_default());
            // Conflicts propagate: the worker retries against the refreshed
            // cache (client-go UpdateConflict semantics).
            client.update(&key, next, resource_version(rs)).await?;
            tracing::debug!(rs = %target_name, replicas = desired, "scaled target rs");
        }
    } else {
        // Compile the template into the target RS (first sighting).
        let mut rs_template = template.clone();
        add_label(&mut rs_template, "pod-template-hash", &short);
        let mut rs_selector = selector.clone();
        if let Some(ml) = rs_selector
            .get_mut("matchLabels")
            .and_then(Value::as_object_mut)
        {
            ml.insert("pod-template-hash".into(), json!(short.clone()));
        }
        let mut rs_labels = template
            .pointer("/metadata/labels")
            .cloned()
            .unwrap_or_else(|| json!({}));
        add_label(&mut rs_labels, "pod-template-hash", &short);
        let rs = json!({
            "apiVersion": "apps/v1",
            "kind": "ReplicaSet",
            "metadata": {
                "name": target_name,
                "namespace": ns,
                "labels": rs_labels,
                "ownerReferences": [owner_reference(dep_name, uid, "Deployment", "apps/v1")],
            },
            "spec": {"replicas": desired, "selector": rs_selector, "template": rs_template},
        });
        match client
            .create(&Key::new(GROUP, "replicasets", ns, &target_name), rs)
            .await
        {
            Ok(_) => tracing::debug!(deployment = dep_name, rs = %target_name, "created target rs"),
            Err(e) if e.is_already_exists() => {} // raced: the next pass scales it
            Err(e) => return Err(e),
        }
    }

    // Drain + delete superseded RSs (owned, not the target).
    for rs in &owned {
        let rs_name = name(rs).unwrap_or_default();
        if rs_name == target_name {
            continue;
        }
        let spec_replicas = rs.pointer("/spec/replicas").and_then(Value::as_u64);
        if spec_replicas.unwrap_or(0) != 0 {
            let mut next = rs.clone();
            next["spec"]["replicas"] = json!(0);
            let key = Key::new(GROUP, "replicasets", ns, rs_name);
            // Conflict -> error: the worker retries via the refreshed cache.
            client.update(&key, next, resource_version(rs)).await?;
            tracing::debug!(rs = rs_name, "draining old rs");
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

    // Aggregate status over the owned RS set (post-mutation read).
    let owned = owned_replicasets(client, ns, dep_name, uid).await?;
    let mut total = 0u64;
    let mut ready = 0u64;
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
        if name(rs) == Some(target_name.as_str()) {
            updated += r;
        }
    }
    let generation = dep
        .pointer("/metadata/generation")
        .and_then(Value::as_u64)
        .unwrap_or(1);
    let new_status = json!({
        "replicas": total,
        "readyReplicas": ready,
        "updatedReplicas": updated,
        "observedGeneration": generation,
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
