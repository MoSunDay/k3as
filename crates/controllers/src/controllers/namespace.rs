//! Namespace lifecycle controller (TODO **T3.1b**, decision **Q20**).
//!
//! A Namespace created through the apiserver carries the `kubernetes`
//! spec finalizer, so a DELETE only stamps `metadata.deletionTimestamp`
//! (soft delete). This controller drains every namespaced kind under a
//! terminating namespace and, once all are gone, performs the terminal
//! delete itself: clear `metadata.finalizers`, then hard delete the
//! Namespace object. Q19 note: in-process controllers bypass the
//! apiserver's finalizer-completion rule (PUT/PATCH with emptied
//! finalizers), so the terminal delete is owned here.
//!
//! Deleting a Deployment cascades to its ReplicaSet/Pods via the GC, but
//! the drain deletes each kind directly anyway -- one pass, no reliance on
//! cross-controller ordering. JSON-only wire (Q10).

use std::sync::Arc;

use serde_json::Value;
use storage::{Key, KeyPrefix};

use crate::client::Client;
use crate::error::ControllerError;
use crate::object::{name, resource_version};

/// `(group, resource)` kinds drained before a Namespace may finalize.
pub(crate) const DRAIN_KINDS: [(&str, &str); 12] = [
    ("", "pods"),
    ("", "services"),
    ("", "endpoints"),
    ("", "configmaps"),
    ("", "secrets"),
    ("", "persistentvolumeclaims"),
    ("apps", "deployments"),
    ("apps", "replicasets"),
    ("apps", "statefulsets"),
    ("apps", "daemonsets"),
    ("apps", "controllerrevisions"),
    ("coordination.k8s.io", "leases"),
];

/// Reconcile one Namespace: no `deletionTimestamp` -> nothing to do;
/// otherwise drain every kind under it, then finalize (clear finalizers +
/// terminal delete). A CAS loss on the finalize write propagates so the
/// worker requeues (retry) -- the object was concurrently modified.
pub async fn reconcile(client: &Arc<dyn Client>, ns: &Value) -> Result<(), ControllerError> {
    let Some(ns_name) = name(ns) else {
        return Ok(());
    };
    if ns.pointer("/metadata/deletionTimestamp").is_none() {
        return Ok(()); // not terminating
    }
    let mut drained = true;
    for (group, resource) in DRAIN_KINDS {
        let objs = client
            .list(&KeyPrefix::new(group, resource, Some(ns_name.to_string())))
            .await?;
        for obj in &objs {
            let Some(obj_name) = name(obj) else { continue };
            let key = Key::new(group, resource, ns_name, obj_name);
            match client.delete(&key).await {
                Ok(()) => {}
                Err(e) if e.is_not_found() => {}
                Err(e) => return Err(e),
            }
        }
        if !objs.is_empty() {
            drained = false; // re-verify next pass (recreating owners race)
        }
    }
    if !drained {
        return Ok(()); // the runner's backstop re-enqueues terminating ns
    }
    // Finalize: empty finalizers, then the terminal delete (Q19/Q20).
    let mut final_state = ns.clone();
    clear_finalizers(&mut final_state);
    let key = Key::new("", "namespaces", "", ns_name);
    client
        .update(&key, final_state, resource_version(ns))
        .await?;
    client.delete(&key).await?;
    tracing::debug!(namespace = ns_name, "namespace drained and finalized");
    Ok(())
}

/// Set `metadata.finalizers = []` (pure mutation; unit-tested). Returns
/// whether the value changed.
pub(crate) fn clear_finalizers(obj: &mut Value) -> bool {
    let Some(meta) = obj.get_mut("metadata").and_then(Value::as_object_mut) else {
        return false;
    };
    let already_empty = meta
        .get("finalizers")
        .and_then(Value::as_array)
        .is_some_and(|a| a.is_empty());
    if already_empty {
        return false;
    }
    meta.insert("finalizers".into(), Value::Array(vec![]));
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn drain_kind_table_covers_the_controller_footprint() {
        assert_eq!(DRAIN_KINDS.len(), 12);
        for kind in [
            ("", "pods"),
            ("", "services"),
            ("", "endpoints"),
            ("", "configmaps"),
            ("", "secrets"),
            ("", "persistentvolumeclaims"),
            ("apps", "deployments"),
            ("apps", "replicasets"),
            ("apps", "statefulsets"),
            ("apps", "daemonsets"),
            ("apps", "controllerrevisions"),
            ("coordination.k8s.io", "leases"),
        ] {
            assert!(DRAIN_KINDS.contains(&kind), "missing {kind:?}");
        }
        // Pods drain before the workload controllers that would recreate
        // them; leases (leader election) drain last.
        let pods = DRAIN_KINDS.iter().position(|k| k == &("", "pods")).unwrap();
        let deps = DRAIN_KINDS
            .iter()
            .position(|k| k == &("apps", "deployments"))
            .unwrap();
        assert!(pods < deps, "pods must drain before deployments");
    }

    #[test]
    fn clear_finalizers_empties_and_is_idempotent() {
        let mut ns = json!({"metadata": {"name": "ns", "finalizers": ["kubernetes"]}});
        assert!(clear_finalizers(&mut ns));
        assert_eq!(ns.pointer("/metadata/finalizers").unwrap(), &json!([]));
        assert!(!clear_finalizers(&mut ns), "second pass is a no-op");
        // Absent finalizers: still normalized to [] (the terminal shape).
        let mut bare = json!({"metadata": {"name": "ns"}});
        assert!(clear_finalizers(&mut bare));
        assert_eq!(bare.pointer("/metadata/finalizers").unwrap(), &json!([]));
    }
}
