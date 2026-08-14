//! Controller set (T3.1a/T3.1b): ReplicaSet / Deployment / Endpoints /
//! StatefulSet / DaemonSet.
//!
//! Scope note: garbage collection is the remaining **T3.1b** work. Each
//! controller is a pure-ish `reconcile(client, object)` function over
//! `serde_json::Value` (JSON-only wire, Q10); desired-state convergence
//! is driven externally by the runner's informers + workqueues (T3.2).

pub mod conditions;
pub mod daemonset;
pub mod deployment;
pub mod endpoints;
/// Pure StatefulSet ordinal/naming/revision math (T3.1b).
pub mod ordinal;
pub mod replicaset;
pub mod rollout;
pub mod statefulset;

use std::sync::Arc;

use serde_json::Value;

use crate::informer::ObjectStore;
use crate::object::controller_of;

/// Informer caches shared by the runner's event fan-out and dispatch.
#[derive(Clone)]
pub struct Caches {
    pub deployments: Arc<ObjectStore>,
    pub replicasets: Arc<ObjectStore>,
    pub statefulsets: Arc<ObjectStore>,
    pub daemonsets: Arc<ObjectStore>,
    pub pods: Arc<ObjectStore>,
    pub services: Arc<ObjectStore>,
    /// Cluster-scoped Nodes: the DaemonSet placement universe (T3.1b);
    /// the node informer keeps it fresh, the node -> DaemonSet fan-out
    /// reads it.
    pub nodes: Arc<ObjectStore>,
}

impl Default for Caches {
    fn default() -> Self {
        Self::new()
    }
}

impl Caches {
    pub fn new() -> Self {
        Self {
            deployments: Arc::new(ObjectStore::new()),
            replicasets: Arc::new(ObjectStore::new()),
            statefulsets: Arc::new(ObjectStore::new()),
            daemonsets: Arc::new(ObjectStore::new()),
            pods: Arc::new(ObjectStore::new()),
            services: Arc::new(ObjectStore::new()),
            nodes: Arc::new(ObjectStore::new()),
        }
    }
}

/// True when `obj`'s controller ownerReference points at `(kind, name)`
/// (or matches `uid` when one is set). Empty uids never match by uid, so
/// objects written without uids (v1 storage has no uid assigner yet) fall
/// back to the precise kind+name test.
pub(crate) fn owned_by(obj: &Value, kind: &str, name: &str, uid: &str) -> bool {
    let Some(owner) = controller_of(obj) else {
        return false;
    };
    let owner_kind = owner.get("kind").and_then(Value::as_str);
    let owner_name = owner.get("name").and_then(Value::as_str);
    let owner_uid = owner.get("uid").and_then(Value::as_str).unwrap_or("");
    (!uid.is_empty() && owner_uid == uid) || (owner_kind == Some(kind) && owner_name == Some(name))
}

/// Objects with `metadata.deletionTimestamp` are terminating: controllers
/// stop counting them (upstream parity).
pub(crate) fn is_terminating(obj: &Value) -> bool {
    obj.pointer("/metadata/deletionTimestamp").is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn with_owner(kind: &str, name: &str, uid: &str) -> Value {
        json!({"metadata": {"ownerReferences": [
            {"kind": kind, "name": name, "uid": uid, "controller": true},
        ]}})
    }

    #[test]
    fn owned_by_matches_uid_then_kind_name() {
        let pod = with_owner("ReplicaSet", "web-1", "u1");
        assert!(owned_by(&pod, "ReplicaSet", "web-1", "u1"));
        assert!(owned_by(&pod, "ReplicaSet", "web-1", "")); // fallback: kind+name
        assert!(!owned_by(&pod, "ReplicaSet", "web-2", ""));
        assert!(!owned_by(&pod, "Deployment", "web-1", ""));
        assert!(!owned_by(&pod, "ReplicaSet", "other", "u2"));
    }

    #[test]
    fn empty_uid_never_matches_other_owners_by_uid() {
        // A bare RS owner (uid "") must NOT be claimed by a deployment that
        // also has no uid.
        let rs_pod = with_owner("ReplicaSet", "bare", "");
        assert!(!owned_by(&rs_pod, "Deployment", "dep", ""));
    }

    #[test]
    fn terminating_detection() {
        assert!(!is_terminating(&json!({"metadata": {}})));
        assert!(is_terminating(
            &json!({"metadata": {"deletionTimestamp": "now"}})
        ));
    }
}
