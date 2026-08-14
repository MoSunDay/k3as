//! Garbage collector (TODO **T3.1b**, decision **Q20**).
//!
//! Two entry points:
//!  - [`sweep`]: list every dependent kind cluster-wide; when an object's
//!    **controller** ownerReference names a *managed* owner kind that no
//!    longer exists (informer cache lookup), delete the dependent. Unknown
//!    owner kinds are skipped (upstream GC only acts on owners it can
//!    verify).
//!  - [`handle_owner_deleted`]: fired from the owner informers' DELETE
//!    events. Default (`Background`, Q20) = immediate cascade via `sweep`;
//!    when the deleted object carries `metadata.annotations[
//!    "init-pro.io/deletion-propagation"] = "Orphan"` (stamped by the
//!    apiserver's `?propagationPolicy=Orphan` DELETE) the dependents keep
//!    living: their matching controller ownerReference is stripped.
//!
//! Cadence: event-driven (owner DELETE) plus a 2s periodic backstop sweep in
//! the runner. V1 race note (Q20, accepted): a just-created owner may be
//! briefly absent from an informer cache, so a sweep can delete -- and its
//! controller then recreate -- a dependent; the system self-heals and no
//! durable state diverges. Fine-grained solid/weak owner graphs, uid-based
//! tombstones and foreground (blocking) deletion are not modeled in v1.
//!
//! JSON-only wire (Q10); pure functions + storage-side effects, no classes.

use std::sync::Arc;

use serde_json::Value;
use storage::{Key, KeyPrefix};

use crate::client::Client;
use crate::controllers::{owned_by, Caches};
use crate::error::ControllerError;
use crate::object::{controller_of, name, namespace, resource_version};

/// Annotation the apiserver stamps on Orphan-propagation deletes (Q20);
/// the emitted DELETE event carries it and the GC branches on it here.
pub(crate) const ORPHAN_ANNOTATION: &str = "init-pro.io/deletion-propagation";

/// `(group, resource)` of every kind the GC sweeps for dangling controller
/// ownerReferences (cluster-wide list).
pub(crate) const DEPENDENT_KINDS: [(&str, &str); 5] = [
    ("", "pods"),
    ("apps", "replicasets"),
    ("", "endpoints"),
    ("", "persistentvolumeclaims"),
    ("apps", "controllerrevisions"),
];

/// Owner kinds the GC is authoritative over (each has an informer cache);
/// any other owner kind on a dependent is left alone.
pub(crate) const MANAGED_OWNER_KINDS: [&str; 5] = [
    "Deployment",
    "ReplicaSet",
    "Service",
    "StatefulSet",
    "DaemonSet",
];

/// One sweep pass: delete dependents whose managed controller owner is gone.
pub async fn sweep(client: &Arc<dyn Client>, caches: &Arc<Caches>) -> Result<(), ControllerError> {
    for (group, resource) in DEPENDENT_KINDS {
        for obj in client.list(&KeyPrefix::new(group, resource, None)).await? {
            let Some(owner) = controller_of(&obj) else {
                continue; // not controlled: not GC-owned
            };
            let (Some(owner_kind), Some(owner_name)) = (
                owner.get("kind").and_then(Value::as_str),
                owner.get("name").and_then(Value::as_str),
            ) else {
                continue;
            };
            if !MANAGED_OWNER_KINDS.contains(&owner_kind) {
                continue; // unknown owner kind: skip (upstream parity)
            }
            let ns = namespace(&obj).unwrap_or("default");
            if owner_in_cache(caches, owner_kind, ns, owner_name) {
                continue;
            }
            let Some(obj_name) = name(&obj) else { continue };
            let key = Key::new(group, resource, ns, obj_name);
            match client.delete(&key).await {
                Ok(()) => tracing::debug!(
                    dependent = %key.as_path(), owner_kind, owner_name,
                    "gc: deleted dangling dependent"
                ),
                Err(e) if e.is_not_found() => {}
                Err(e) => return Err(e),
            }
        }
    }
    Ok(())
}

/// Owner-DELETE hook: orphan (strip matching ownerReferences) or cascade.
/// Fire-and-forget by design -- the runner spawns this detached from the
/// informer's sync event handler, so failures are logged, not propagated.
pub async fn handle_owner_deleted(client: &Arc<dyn Client>, caches: &Arc<Caches>, owner: &Value) {
    if is_orphaned(owner) {
        if let Err(e) = orphan_dependents(client, owner).await {
            tracing::warn!(error = %e, "gc: orphan pass failed (backstop sweep follows)");
        }
    } else if let Err(e) = sweep(client, caches).await {
        tracing::warn!(error = %e, "gc: cascade sweep failed (backstop follows)");
    }
}

/// True when the deleted owner requested Orphan propagation (Q20).
pub(crate) fn is_orphaned(owner: &Value) -> bool {
    owner
        .get("metadata")
        .and_then(|m| m.get("annotations"))
        .and_then(|a| a.get(ORPHAN_ANNOTATION))
        .and_then(Value::as_str)
        == Some("Orphan")
}

/// Strip the owner's controller ownerReference from every dependent
/// (same-namespace, kind+name, uid when set); other references survive.
async fn orphan_dependents(client: &Arc<dyn Client>, owner: &Value) -> Result<(), ControllerError> {
    let Some(owner_name) = name(owner) else {
        return Ok(());
    };
    let Some(owner_kind) = owner.get("kind").and_then(Value::as_str) else {
        return Ok(());
    };
    let owner_ns = namespace(owner).unwrap_or("default");
    let owner_uid = owner
        .pointer("/metadata/uid")
        .and_then(Value::as_str)
        .unwrap_or("");
    for (group, resource) in DEPENDENT_KINDS {
        for obj in client.list(&KeyPrefix::new(group, resource, None)).await? {
            let ns = namespace(&obj).unwrap_or("default");
            if ns != owner_ns || !owned_by(&obj, owner_kind, owner_name, owner_uid) {
                continue;
            }
            let Some(obj_name) = name(&obj) else { continue };
            let mut next = obj.clone();
            if !strip_controller_owner_ref(&mut next, owner_kind, owner_name, owner_uid) {
                continue;
            }
            let key = Key::new(group, resource, ns, obj_name);
            client.update(&key, next, resource_version(&obj)).await?;
            tracing::debug!(dependent = %key.as_path(), "gc: orphaned dependent");
        }
    }
    Ok(())
}

/// Remove the controller ownerReference matching `(kind, name[, uid])` in
/// place. Returns true when the array shrank. Pure (unit-tested).
pub(crate) fn strip_controller_owner_ref(
    obj: &mut Value,
    kind: &str,
    owner_name: &str,
    uid: &str,
) -> bool {
    let Some(refs) = obj
        .pointer_mut("/metadata/ownerReferences")
        .and_then(Value::as_array_mut)
    else {
        return false;
    };
    let before = refs.len();
    let matches = |r: &Value| {
        let r_uid = r.get("uid").and_then(Value::as_str).unwrap_or("");
        let by_uid = !uid.is_empty() && r_uid == uid;
        let by_identity = r.get("kind").and_then(Value::as_str) == Some(kind)
            && r.get("name").and_then(Value::as_str) == Some(owner_name);
        by_uid || by_identity
    };
    refs.retain(|r| !(r.get("controller").and_then(Value::as_bool) == Some(true) && matches(r)));
    before != refs.len()
}

/// Cache lookup for one managed owner kind (`owner_in_cache` == exists).
fn owner_in_cache(caches: &Caches, kind: &str, ns: &str, owner_name: &str) -> bool {
    match kind {
        "Deployment" => caches.deployments.get(ns, owner_name).is_some(),
        "ReplicaSet" => caches.replicasets.get(ns, owner_name).is_some(),
        "Service" => caches.services.get(ns, owner_name).is_some(),
        "StatefulSet" => caches.statefulsets.get(ns, owner_name).is_some(),
        "DaemonSet" => caches.daemonsets.get(ns, owner_name).is_some(),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn managed_owner_kind_table() {
        for kind in MANAGED_OWNER_KINDS {
            assert!(matches!(
                kind,
                "Deployment" | "ReplicaSet" | "Service" | "StatefulSet" | "DaemonSet"
            ));
        }
        assert!(!MANAGED_OWNER_KINDS.contains(&"Node"));
        assert!(!MANAGED_OWNER_KINDS.contains(&("Job")));
    }

    #[test]
    fn dependent_kinds_are_the_sweep_universe() {
        assert_eq!(DEPENDENT_KINDS.len(), 5);
        assert!(DEPENDENT_KINDS.contains(&("", "pods")));
        assert!(DEPENDENT_KINDS.contains(&("apps", "replicasets")));
        assert!(DEPENDENT_KINDS.contains(&("", "endpoints")));
        assert!(DEPENDENT_KINDS.contains(&("", "persistentvolumeclaims")));
        assert!(DEPENDENT_KINDS.contains(&("apps", "controllerrevisions")));
        // The drain kinds (namespace.rs) are a superset for teardown; the
        // sweep universe only needs owner-carrying kinds.
        for kind in DEPENDENT_KINDS {
            assert!(!kind.0.is_empty() || !kind.1.is_empty());
        }
    }

    #[test]
    fn unknown_owner_kind_is_recognized_as_unmanaged() {
        // sweep()'s guard expressed through the table: a Job-owned pod must
        // never be judged by these caches.
        assert!(!MANAGED_OWNER_KINDS.contains(&"Job"));
        let caches = Caches::new();
        assert!(!owner_in_cache(&caches, "Job", "default", "any"));
    }

    #[test]
    fn orphan_annotation_detection() {
        let orphan = json!({"kind": "Deployment", "metadata": {
            "annotations": {"init-pro.io/deletion-propagation": "Orphan"}}});
        assert!(is_orphaned(&orphan));
        let cascade = json!({"kind": "Deployment", "metadata": {
            "annotations": {"init-pro.io/deletion-propagation": "Background"}}});
        assert!(!is_orphaned(&cascade));
        assert!(!is_orphaned(&json!({"kind": "Deployment", "metadata": {}})));
    }

    #[test]
    fn strip_owner_ref_removes_only_the_controller_match() {
        let mut rs = json!({"metadata": {"name": "web-1", "ownerReferences": [
            {"kind": "Deployment", "name": "web", "uid": "", "controller": true},
            {"kind": "Whatever", "name": "other", "controller": false},
        ]}});
        assert!(strip_controller_owner_ref(&mut rs, "Deployment", "web", ""));
        assert_eq!(
            rs.pointer("/metadata/ownerReferences").unwrap(),
            &json!([{"kind": "Whatever", "name": "other", "controller": false}])
        );
        // Idempotent on a second pass.
        assert!(!strip_controller_owner_ref(
            &mut rs,
            "Deployment",
            "web",
            ""
        ));
    }

    #[test]
    fn strip_owner_ref_matches_by_uid_when_present() {
        let mut pod = json!({"metadata": {"ownerReferences": [
            {"kind": "ReplicaSet", "name": "renamed", "uid": "u1", "controller": true},
        ]}});
        assert!(strip_controller_owner_ref(
            &mut pod,
            "ReplicaSet",
            "old-name",
            "u1"
        ));
        assert_eq!(
            pod.pointer("/metadata/ownerReferences").unwrap(),
            &json!([])
        );
    }

    #[test]
    fn strip_owner_ref_without_references_is_a_noop() {
        let mut bare = json!({"metadata": {"name": "x"}});
        assert!(!strip_controller_owner_ref(
            &mut bare,
            "Deployment",
            "web",
            ""
        ));
    }
}
