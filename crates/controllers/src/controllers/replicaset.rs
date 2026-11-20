//! ReplicaSet controller (T3.1a; status extended in T3.1b).
//!
//! Level-driven pod reconciliation: `owned - desired` creates pods from
//! `spec.template`; surplus pods are deleted preferring unscheduled ones
//! (upstream victim ordering); `status.replicas`/`fullyLabeledReplicas`/
//! `readyReplicas`/`availableReplicas` are written only on change
//! (anti-oscillation). Ready/available both count owned, non-terminating
//! pods that are [`crate::object::pod_is_ready`] -- pods are ready-by-
//! default without kubelet (T4.2), so an explicit not-Ready condition is
//! the only way a pod drops out of the counts.

use serde_json::{json, Map, Value};
use storage::{Key, KeyPrefix};

use crate::client::Client;
use crate::controllers::{is_terminating, owned_by};
use crate::error::ControllerError;
use crate::object::{
    name, namespace, owner_reference, pod_is_ready, resource_version, selector_matches, semantic_eq,
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
            // Template annotations propagate into created pods (upstream
            // parity): the workload author's annotations — e.g. the
            // host-direct request `init-pro.io/rootfs: /` (T4.15, Q60) —
            // must reach the pod the controller materializes. Only the
            // annotations key is copied, so controller-owned metadata
            // (name/labels/ownerReferences above) is never clobbered.
            if let Some(annotations) = template
                .pointer("/metadata/annotations")
                .filter(|v| v.is_object() && !v.as_object().is_some_and(Map::is_empty))
            {
                meta["annotations"] = annotations.clone();
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
    // Ready/available: owned, non-terminating, pod_is_ready pods (T3.1b).
    // Without kubelet pods carry no conditions and default to ready, so
    // these normally equal `replicas`; an explicit Ready=False drops a pod.
    let ready = owned.iter().filter(|p| pod_is_ready(p)).count() as u64;
    let new_status = json!({
        "replicas": count,
        "fullyLabeledReplicas": labeled,
        "readyReplicas": ready,
        "availableReplicas": ready,
    });
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::StorageClient;
    use std::sync::Arc;
    use storage::EmbeddedStorage;

    /// One RS whose template carries labels AND annotations (the host-direct
    /// request shape, T4.15/Q60).
    fn rs_with_template(annotations: Value) -> Value {
        json!({
            "apiVersion": "apps/v1",
            "kind": "ReplicaSet",
            "metadata": {"name": "web", "namespace": "default", "uid": "u1"},
            "spec": {
                "replicas": 1,
                "selector": {"matchLabels": {"app": "web"}},
                "template": {
                    "metadata": {"labels": {"app": "web"}, "annotations": annotations},
                    "spec": {"containers": [{"name": "c", "image": "nginx"}]},
                },
            },
        })
    }

    async fn created_pods(client: &Arc<dyn Client>) -> Vec<Value> {
        client
            .list(&KeyPrefix::new("", "pods", Some("default".into())))
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn template_annotations_propagate_into_created_pods() {
        let store = Arc::new(EmbeddedStorage::new());
        let client: Arc<dyn Client> = Arc::new(StorageClient::new(store));
        let rs = rs_with_template(json!({"init-pro.io/rootfs": "/", "team": "ai"}));
        client
            .create(
                &Key::new("apps", "replicasets", "default", "web"),
                rs.clone(),
            )
            .await
            .unwrap();
        reconcile(&client, &rs).await.unwrap();

        let pods = created_pods(&client).await;
        assert_eq!(pods.len(), 1);
        let pod = &pods[0];
        // Annotations copied verbatim from the template.
        assert_eq!(pod["metadata"]["annotations"]["init-pro.io/rootfs"], "/");
        assert_eq!(pod["metadata"]["annotations"]["team"], "ai");
        // Controller-owned metadata is intact and never clobbered.
        assert_eq!(pod["metadata"]["namespace"], "default");
        assert!(pod["metadata"]["name"]
            .as_str()
            .unwrap()
            .starts_with("web-"));
        assert_eq!(pod["metadata"]["labels"]["app"], "web");
        assert_eq!(pod["metadata"]["ownerReferences"][0]["kind"], "ReplicaSet");
        assert_eq!(pod["spec"]["containers"][0]["image"], "nginx");
    }

    #[tokio::test]
    async fn templates_without_annotations_create_unannotated_pods() {
        let store = Arc::new(EmbeddedStorage::new());
        let client: Arc<dyn Client> = Arc::new(StorageClient::new(store));
        let rs = rs_with_template(json!({}));
        client
            .create(
                &Key::new("apps", "replicasets", "default", "web"),
                rs.clone(),
            )
            .await
            .unwrap();
        reconcile(&client, &rs).await.unwrap();

        let pods = created_pods(&client).await;
        assert_eq!(pods.len(), 1);
        assert!(pods[0]["metadata"].get("annotations").is_none());
        assert_eq!(pods[0]["metadata"]["labels"]["app"], "web");
    }
}
