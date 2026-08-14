//! StatefulSet controller (TODO **T3.1b**).
//!
//! Pods named `<sts>-<ordinal>` are created in ordinal order; one PVC per
//! `spec.volumeClaimTemplates` entry per ordinal below `spec.replicas`
//! (decision **Q22**: lazy storage, no binder until T6.2); every distinct
//! pod template is recorded as an `apps/v1` ControllerRevision
//! (`<name>-<hash10>` of `crate::id::template_hash`, bumped `revision`).
//! `podManagementPolicy`: `OrderedReady` (default) gates ordinal i on pod
//! i-1 existing AND ready ([`crate::object::pod_is_ready`]); `Parallel`
//! creates unconditionally. `updateStrategy`: `RollingUpdate` (default)
//! deletes stale-revision pods at ordinals >= `rollingUpdate.partition`,
//! highest first (the create path recreates them at the new revision);
//! `OnDelete` never deletes for update reasons. Status is write-if-changed
//! via `semantic_eq` (the CAS pattern shared with the Deployment
//! controller). v1 simplifications: scale-down deletes ALL surplus pods at
//! once (upstream one at a time); PVCs are NEVER deleted
//! (`persistentVolumeClaimRetentionPolicy: Retain`; the STS's own deletion
//! leaves them to GC); old ControllerRevisions are kept forever (no
//! `revisionHistoryLimit`). JSON-only wire (Q10); in-process storage (Q19).

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::{json, Value};
use storage::{Key, KeyPrefix};

use crate::client::Client;
use crate::controllers::ordinal::{
    next_revision, ordinal_of, pod_name, pvc_name, revision_hash_of, should_delete,
    REVISION_HASH_LABEL,
};
use crate::controllers::{is_terminating, owned_by};
use crate::error::ControllerError;
use crate::id::{add_label, template_hash};
use crate::object::{
    name, namespace, owner_reference, pod_is_ready, resource_version, semantic_eq,
};

const POD_INDEX_LABEL: &str = "apps.kubernetes.io/pod-index";

/// Reconcile one StatefulSet: claims -> ordered pods -> revision -> status.
pub async fn reconcile(client: &Arc<dyn Client>, sts: &Value) -> Result<(), ControllerError> {
    let ns = namespace(sts).unwrap_or("default");
    let Some(sts_name) = name(sts) else {
        return Ok(()); // unparseable object: nothing to converge
    };
    if is_terminating(sts) {
        return Ok(());
    }
    let template = sts
        .pointer("/spec/template")
        .cloned()
        .unwrap_or(Value::Null);
    if !template.is_object() {
        return Ok(()); // no pod template: inert (upstream validation error)
    }
    let uid = sts
        .pointer("/metadata/uid")
        .and_then(Value::as_str)
        .unwrap_or("");
    let desired = sts
        .pointer("/spec/replicas")
        .and_then(Value::as_i64)
        .unwrap_or(1)
        .max(0);
    let service_name = sts.pointer("/spec/serviceName").and_then(Value::as_str);
    // OrderedReady is the default policy; RollingUpdate the default strategy.
    let parallel = sts
        .pointer("/spec/podManagementPolicy")
        .and_then(Value::as_str)
        == Some("Parallel");
    let strategy = sts.pointer("/spec/updateStrategy");
    let rolling = strategy.and_then(|s| s.get("type")).and_then(Value::as_str) != Some("OnDelete");
    let partition = strategy
        .and_then(|s| s.pointer("/rollingUpdate/partition"))
        .and_then(Value::as_i64)
        .unwrap_or(0)
        .max(0);
    let claim_templates = sts
        .pointer("/spec/volumeClaimTemplates")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let hash = template_hash(&template);
    let short = &hash[..10];
    let revision_name = format!("{sts_name}-{short}");

    // Owned, live pods keyed by ordinal (foreign/non-ordinal names ignored).
    let mut by_ordinal: HashMap<i64, Value> = HashMap::new();
    let prefix = KeyPrefix::new("", "pods", Some(ns.to_string()));
    for pod in client.list(&prefix).await? {
        if !is_terminating(&pod) && owned_by(&pod, "StatefulSet", sts_name, uid) {
            if let Some(o) = ordinal_of(&pod, sts_name) {
                by_ordinal.insert(o, pod);
            }
        }
    }

    // PVCs (Q22 lazy storage): claim objects per template per live ordinal.
    ensure_claims(client, ns, sts_name, uid, &claim_templates, desired).await?;

    // Deletes first (highest ordinal): scale-down surplus + stale revisions.
    let mut ordinals: Vec<i64> = by_ordinal.keys().copied().collect();
    ordinals.sort_unstable_by(|a, b| b.cmp(a));
    for ordinal in ordinals {
        let Some(pod) = by_ordinal.get(&ordinal) else {
            continue;
        };
        if !should_delete(
            ordinal,
            desired,
            rolling,
            partition,
            revision_hash_of(pod),
            short,
        ) {
            continue;
        }
        let victim = name(pod).unwrap_or_default().to_string();
        by_ordinal.remove(&ordinal);
        client
            .delete(&Key::new("", "pods", ns, victim.as_str()))
            .await?;
        tracing::debug!(sts = sts_name, pod = %victim, "deleted statefulset pod");
    }

    // Creates in ordinal order; OrderedReady gates ordinal i on pod i-1.
    let mut prev_ready = true; // ordinal 0 has no predecessor
    for ordinal in 0..desired {
        match by_ordinal.get(&ordinal) {
            Some(pod) => prev_ready = pod_is_ready(pod),
            None => {
                if !parallel && !prev_ready {
                    break; // OrderedReady: wait for i-1 to be ready
                }
                let pod = pod_body(&template, ns, sts_name, uid, ordinal, short, service_name);
                let key = Key::new("", "pods", ns, pod_name(sts_name, ordinal).as_str());
                match client.create(&key, pod.clone()).await {
                    Ok(_) => tracing::debug!(sts = sts_name, ordinal, "created statefulset pod"),
                    Err(e) if e.is_already_exists() => {} // recreate race: requeue converges
                    Err(e) => return Err(e),
                }
                by_ordinal.insert(ordinal, pod);
                prev_ready = true; // fresh pod: no conditions -> ready
            }
        }
    }

    ensure_revision(client, ns, sts_name, uid, &template, &revision_name, short).await?;

    // Status write-if-changed, computed from the post-mutation state.
    let replicas = by_ordinal.len() as u64;
    let ready = by_ordinal.values().filter(|p| pod_is_ready(p)).count() as u64;
    let updated = by_ordinal
        .values()
        .filter(|p| revision_hash_of(p) == Some(short))
        .count() as u64;
    let fallback = sts
        .pointer("/status/currentRevision")
        .and_then(Value::as_str)
        .unwrap_or(&revision_name)
        .to_string();
    let current_revision = if updated == replicas {
        revision_name.clone()
    } else {
        fallback
    };
    let observed = sts
        .pointer("/metadata/generation")
        .and_then(Value::as_u64)
        .unwrap_or(1);
    let new_status = json!({
        "replicas": replicas,
        "readyReplicas": ready,
        "availableReplicas": ready,
        "updatedReplicas": updated,
        "currentRevision": current_revision,
        "updateRevision": revision_name,
        "observedGeneration": observed,
    });
    let current = sts.get("status").cloned().unwrap_or(json!({}));
    if !semantic_eq(&current, &new_status) {
        let mut next = sts.clone();
        next["status"] = new_status;
        let key = Key::new("apps", "statefulsets", ns, sts_name);
        // Conflict propagates: the worker retries against the fresh cache.
        client.update(&key, next, resource_version(sts)).await?;
        tracing::debug!(sts = sts_name, replicas, "statefulset status updated");
    }
    Ok(())
}

/// The pod object for `ordinal`: template labels + identity labels, spec +
/// `hostname`/`subdomain`, and the STS controller ownerReference.
fn pod_body(
    template: &Value,
    ns: &str,
    sts_name: &str,
    uid: &str,
    ordinal: i64,
    short: &str,
    service_name: Option<&str>,
) -> Value {
    let pod_name = pod_name(sts_name, ordinal);
    let mut meta = json!({
        "name": pod_name,
        "namespace": ns,
        "ownerReferences": [owner_reference(sts_name, uid, "StatefulSet", "apps/v1")],
    });
    if let Some(labels) = template
        .pointer("/metadata/labels")
        .filter(|v| v.is_object())
    {
        meta["labels"] = labels.clone();
    }
    let mut spec = template
        .pointer("/spec")
        .filter(|v| v.is_object())
        .cloned()
        .unwrap_or(json!({}));
    spec["hostname"] = json!(pod_name);
    if let Some(svc) = service_name {
        spec["subdomain"] = json!(svc);
    }
    let mut pod = json!({"apiVersion": "v1", "kind": "Pod", "metadata": meta, "spec": spec});
    add_label(&mut pod, REVISION_HASH_LABEL, short);
    add_label(&mut pod, POD_INDEX_LABEL, &ordinal.to_string());
    pod
}

/// Create `{claim}-{sts}-{ordinal}` PVCs for every ordinal below `desired`
/// that lacks one. NEVER deletes (Retain; see the module doc).
async fn ensure_claims(
    client: &Arc<dyn Client>,
    ns: &str,
    sts_name: &str,
    uid: &str,
    claim_templates: &[Value],
    desired: i64,
) -> Result<(), ControllerError> {
    for ct in claim_templates {
        let Some(claim) = ct.pointer("/metadata/name").and_then(Value::as_str) else {
            continue;
        };
        for ordinal in 0..desired {
            let pvc = pvc_name(claim, sts_name, ordinal);
            let key = Key::new("", "persistentvolumeclaims", ns, pvc.as_str());
            if client.get(&key).await?.is_some() {
                continue;
            }
            let obj = json!({
                "apiVersion": "v1",
                "kind": "PersistentVolumeClaim",
                "metadata": {
                    "name": pvc,
                    "namespace": ns,
                    "labels": ct.pointer("/metadata/labels")
                        .filter(|v| v.is_object())
                        .cloned()
                        .unwrap_or(json!({})),
                    "ownerReferences": [owner_reference(sts_name, uid, "StatefulSet", "apps/v1")],
                },
                "spec": ct.pointer("/spec").cloned().unwrap_or(json!({})),
            });
            match client.create(&key, obj).await {
                Ok(_) => tracing::debug!(sts = sts_name, pvc = %pvc, "created pvc"),
                Err(e) if e.is_already_exists() => {}
                Err(e) => return Err(e),
            }
        }
    }
    Ok(())
}

/// Record `revision_name` (the current template) if missing; old revisions
/// are kept (no revisionHistoryLimit in v1).
async fn ensure_revision(
    client: &Arc<dyn Client>,
    ns: &str,
    sts_name: &str,
    uid: &str,
    template: &Value,
    revision_name: &str,
    short: &str,
) -> Result<(), ControllerError> {
    let prefix = KeyPrefix::new("apps", "controllerrevisions", Some(ns.to_string()));
    let existing: Vec<Value> = client
        .list(&prefix)
        .await?
        .into_iter()
        .filter(|rev| owned_by(rev, "StatefulSet", sts_name, uid))
        .collect();
    if existing.iter().any(|rev| name(rev) == Some(revision_name)) {
        return Ok(());
    }
    let revision = next_revision(&existing);
    let obj = json!({
        "apiVersion": "apps/v1",
        "kind": "ControllerRevision",
        "metadata": {
            "name": revision_name,
            "namespace": ns,
            "labels": {REVISION_HASH_LABEL: short},
            "ownerReferences": [owner_reference(sts_name, uid, "StatefulSet", "apps/v1")],
        },
        "data": template.clone(),
        "revision": revision,
    });
    let key = Key::new("apps", "controllerrevisions", ns, revision_name);
    match client.create(&key, obj).await {
        Ok(_) => tracing::debug!(sts = sts_name, revision, "controllerrevision recorded"),
        Err(e) if e.is_already_exists() => {}
        Err(e) => return Err(e),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn pod_body_sets_identity_labels_hostname_and_owner() {
        let template = json!({
            "metadata": {"labels": {"app": "web"}},
            "spec": {"containers": [{"name": "c", "image": "nginx"}]},
        });
        let pod = pod_body(
            &template,
            "default",
            "web",
            "u1",
            2,
            "abcd1234ef",
            Some("web-svc"),
        );
        assert_eq!(pod["metadata"]["name"], "web-2");
        assert_eq!(pod["metadata"]["namespace"], "default");
        assert_eq!(pod["metadata"]["labels"]["app"], "web");
        assert_eq!(pod["metadata"]["labels"][REVISION_HASH_LABEL], "abcd1234ef");
        assert_eq!(pod["metadata"]["labels"][POD_INDEX_LABEL], "2");
        assert_eq!(pod["spec"]["hostname"], "web-2");
        assert_eq!(pod["spec"]["subdomain"], "web-svc");
        assert_eq!(pod["spec"]["containers"][0]["image"], "nginx");
        assert_eq!(pod["metadata"]["ownerReferences"][0]["kind"], "StatefulSet");
        assert_eq!(pod["metadata"]["ownerReferences"][0]["controller"], true);
    }
}
