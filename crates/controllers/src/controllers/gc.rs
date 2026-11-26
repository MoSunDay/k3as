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
//! A3 (P2 hardening): `persistentvolumeclaims` is NOT in the sweep universe.
//! StatefulSet claims carry a controller ownerRef for ownership
//! bookkeeping, but upstream keeps them across STS deletion
//! (`persistentVolumeClaimRetentionPolicy` defaults to Retain) -- sweeping
//! them was silent data loss.
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
/// ownerReferences (cluster-wide list). A5: `init-pro.io/agents` (Workflow
/// step Agent children) join the sweep universe (Q35); T8.3/Q56 adds
/// `init-pro.io/builds` (ServicePipeline children); T8.5/Q58 adds the canary
/// `apps/deployments` + core `services` children a pipeline owns.
pub(crate) const DEPENDENT_KINDS: [(&str, &str); 9] = [
    ("", "pods"),
    ("apps", "replicasets"),
    ("", "endpoints"),
    // A3: `persistentvolumeclaims` is deliberately NOT swept. StatefulSet
    // PVCs carry a controller ownerRef for ownership bookkeeping, but
    // upstream keeps them alive across STS deletion
    // (`persistentVolumeClaimRetentionPolicy` defaults to Retain).
    ("apps", "controllerrevisions"),
    ("argoproj.io", "workflows"),
    ("init-pro.io", "agents"),
    ("init-pro.io", "builds"),
    ("apps", "deployments"),
    ("", "services"),
];

/// Owner kinds the GC is authoritative over (each has an informer cache);
/// any other owner kind on a dependent is left alone.
pub(crate) const MANAGED_OWNER_KINDS: [&str; 8] = [
    "Deployment",
    "ReplicaSet",
    "Service",
    "StatefulSet",
    "DaemonSet",
    "Workflow",
    "CronWorkflow",
    "ServicePipeline",
];

/// One sweep pass: delete dependents whose managed controller owner is gone.
///
/// A2 guard: if ANY owner cache has not finished its first LIST, the pass
/// is skipped entirely. Both entry points (owner-DELETE hook and the 2s
/// backstop) land here, so a cold-started manager cannot sweep against an
/// empty cache.
pub async fn sweep(client: &Arc<dyn Client>, caches: &Arc<Caches>) -> Result<(), ControllerError> {
    if !caches.is_synced() {
        tracing::debug!("gc: caches not synced yet; skipping sweep");
        return Ok(());
    }
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
            let owner_uid = owner.get("uid").and_then(Value::as_str).unwrap_or("");
            if !MANAGED_OWNER_KINDS.contains(&owner_kind) {
                continue; // unknown owner kind: skip (upstream parity)
            }
            let ns = namespace(&obj).unwrap_or("default");
            // A4: the cached owner must be the SAME object, not merely a
            // same-named successor -- a deleted-then-recreated owner leaves
            // its predecessor's dependents dangling.
            if owner_in_cache(caches, owner_kind, ns, owner_name, owner_uid) {
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

/// Cache lookup for one managed owner kind: the cached owner object (the
/// sweep needs `/metadata/uid`, not just existence).
fn cached_owner(caches: &Caches, kind: &str, ns: &str, owner_name: &str) -> Option<Value> {
    match kind {
        "Deployment" => caches.deployments.get(ns, owner_name),
        "ReplicaSet" => caches.replicasets.get(ns, owner_name),
        "Service" => caches.services.get(ns, owner_name),
        "StatefulSet" => caches.statefulsets.get(ns, owner_name),
        "DaemonSet" => caches.daemonsets.get(ns, owner_name),
        "Workflow" => caches.workflows.get(ns, owner_name),
        "CronWorkflow" => caches.cronworkflows.get(ns, owner_name),
        "ServicePipeline" => caches.servicepipelines.get(ns, owner_name),
        _ => None,
    }
    .map(|o| o.as_ref().clone())
}

/// A4: does the cached owner satisfy the dependent's ownerReference uid?
///
/// An ownerReference without a uid (or an owner object stored without one)
/// falls back to the kind+name identity that placed it in the cache -- the
/// same convention as [`crate::controllers::owned_by`]. Pure (unit-tested).
pub(crate) fn owner_alive(cached_owner: &Value, owner_uid: &str) -> bool {
    if owner_uid.is_empty() {
        return true;
    }
    cached_owner
        .pointer("/metadata/uid")
        .and_then(Value::as_str)
        .is_some_and(|uid| uid == owner_uid)
}

/// `owner_in_cache` == a live owner (A4: same name AND same uid).
fn owner_in_cache(
    caches: &Caches,
    kind: &str,
    ns: &str,
    owner_name: &str,
    owner_uid: &str,
) -> bool {
    cached_owner(caches, kind, ns, owner_name).is_some_and(|o| owner_alive(&o, owner_uid))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::StorageClient;
    // Q42: the workflow builders moved to the arust-workflow engine.
    use serde_json::json;
    use std::sync::Arc;
    use storage::{EmbeddedStorage, StoredEntry, WatchEvent};
    use workflow_engine::agent::build_agent;
    use workflow_engine::plan::{PlannedStep, StepTarget};

    #[test]
    fn managed_owner_kind_table() {
        for kind in MANAGED_OWNER_KINDS {
            assert!(matches!(
                kind,
                "Deployment"
                    | "ReplicaSet"
                    | "Service"
                    | "StatefulSet"
                    | "DaemonSet"
                    | "Workflow"
                    | "CronWorkflow"
                    | "ServicePipeline"
            ));
        }
        assert!(!MANAGED_OWNER_KINDS.contains(&"Node"));
        assert!(!MANAGED_OWNER_KINDS.contains(&("Job")));
    }

    #[test]
    fn dependent_kinds_are_the_sweep_universe() {
        assert_eq!(DEPENDENT_KINDS.len(), 9);
        assert!(DEPENDENT_KINDS.contains(&("", "pods")));
        assert!(DEPENDENT_KINDS.contains(&("apps", "replicasets")));
        assert!(DEPENDENT_KINDS.contains(&("", "endpoints")));
        // A3: PVCs are Retain -- a StatefulSet delete must NOT cascade to
        // its claims, so they stay out of the sweep universe.
        assert!(!DEPENDENT_KINDS.contains(&("", "persistentvolumeclaims")));
        assert!(DEPENDENT_KINDS.contains(&("apps", "controllerrevisions")));
        assert!(DEPENDENT_KINDS.contains(&("argoproj.io", "workflows")));
        assert!(DEPENDENT_KINDS.contains(&("init-pro.io", "agents")));
        assert!(DEPENDENT_KINDS.contains(&("init-pro.io", "builds")));
        // T8.5/Q58: the canary Deployment/Service children a pipeline owns.
        assert!(DEPENDENT_KINDS.contains(&("apps", "deployments")));
        assert!(DEPENDENT_KINDS.contains(&("", "services")));
        // The drain kinds (namespace.rs) are a superset for teardown; the
        // sweep universe only needs owner-carrying kinds.
        for kind in DEPENDENT_KINDS {
            assert!(!kind.0.is_empty() || !kind.1.is_empty());
        }
    }

    /// A3 regression: StatefulSet claims are Retain (upstream
    /// `persistentVolumeClaimRetentionPolicy` default) -- a sweep must never
    /// delete a PVC, even one whose controller ownerReference is dangling.
    #[tokio::test]
    async fn sweep_never_deletes_statefulset_claims() {
        let client: Arc<dyn Client> =
            Arc::new(StorageClient::new(Arc::new(EmbeddedStorage::new())));
        let pvc_key = Key::new("", "persistentvolumeclaims", "default", "data-web-0");
        client
            .create(
                &pvc_key,
                json!({"apiVersion": "v1", "kind": "PersistentVolumeClaim",
                       "metadata": {"name": "data-web-0", "namespace": "default",
                                    "ownerReferences": [{"kind": "StatefulSet", "name": "web",
                                                         "uid": "u-sts", "controller": true}]},
                       "spec": {}}),
            )
            .await
            .unwrap();
        let caches = Arc::new(Caches::new());
        sweep(&client, &caches).await.unwrap();
        assert!(
            client.get(&pvc_key).await.unwrap().is_some(),
            "A3: PVCs must survive their StatefulSet (Retain)"
        );
    }

    #[test]
    fn unknown_owner_kind_is_recognized_as_unmanaged() {
        // sweep()'s guard expressed through the table: a Job-owned pod must
        // never be judged by these caches.
        assert!(!MANAGED_OWNER_KINDS.contains(&"Job"));
        let caches = Caches::new();
        assert!(!owner_in_cache(&caches, "Job", "default", "any", ""));
    }

    /// A4: the uid comparison is the identity test; an ownerReference (or a
    /// cached object) without a uid falls back to kind+name, matching
    /// `owned_by`.
    #[test]
    fn owner_alive_requires_the_same_uid() {
        let owner = json!({"metadata": {"name": "web", "uid": "u-new"}});
        assert!(owner_alive(&owner, "u-new"));
        assert!(!owner_alive(&owner, "u-old"));
        // No uid on the reference -> name identity is all we have.
        assert!(owner_alive(&owner, ""));
        // Reference names a uid the cached object does not carry -> stale.
        assert!(!owner_alive(&json!({"metadata": {"name": "web"}}), "u-new"));
    }

    /// A4 regression: an owner deleted and recreated under the same name is
    /// a different object -- its predecessor's dependents must be swept,
    /// while a dependent pointing at the live uid survives.
    #[tokio::test]
    async fn sweep_compares_the_owner_uid_not_only_the_name() {
        let client: Arc<dyn Client> =
            Arc::new(StorageClient::new(Arc::new(EmbeddedStorage::new())));
        let caches = Arc::new(Caches::new());
        caches
            .deployments
            .apply_event(&WatchEvent::Put(Arc::new(StoredEntry {
                key: "/registry/deployments/default/web".into(),
                value: json!({"apiVersion": "apps/v1", "kind": "Deployment",
                          "metadata": {"name": "web", "namespace": "default",
                                       "uid": "u-new"}}),
                create_revision: 1,
                mod_revision: 1,
                version: 1,
            })));
        let rs_key = Key::new("apps", "replicasets", "default", "web-1");
        client
            .create(
                &rs_key,
                json!({"apiVersion": "apps/v1", "kind": "ReplicaSet",
                       "metadata": {"name": "web-1", "namespace": "default",
                                    "ownerReferences": [{"kind": "Deployment", "name": "web",
                                                         "uid": "u-old", "controller": true}]}}),
            )
            .await
            .unwrap();
        sweep(&client, &caches).await.unwrap();
        assert!(
            client.get(&rs_key).await.unwrap().is_none(),
            "A4: a same-name owner with a new uid must not keep stale dependents"
        );

        // Same uid -> the owner is really there: the dependent survives.
        let live_key = Key::new("apps", "replicasets", "default", "web-2");
        client
            .create(
                &live_key,
                json!({"apiVersion": "apps/v1", "kind": "ReplicaSet",
                       "metadata": {"name": "web-2", "namespace": "default",
                                    "ownerReferences": [{"kind": "Deployment", "name": "web",
                                                         "uid": "u-new", "controller": true}]}}),
            )
            .await
            .unwrap();
        sweep(&client, &caches).await.unwrap();
        assert!(
            client.get(&live_key).await.unwrap().is_some(),
            "a dependent owned by the live uid is kept"
        );
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

    /// A5/Q35 acceptance: an `init-pro.io/agents` Agent child carrying a
    /// Workflow controller ownerReference is swept exactly when the owner
    /// left the informer caches (empty workflow cache -> deleted; owner
    /// present -> survives). Real `EmbeddedStorage` + `StorageClient`, no
    /// runner harness needed -- `sweep` is the cascade entry point.
    #[tokio::test]
    async fn sweep_cascades_agent_children_only_when_the_owner_is_gone() {
        let store = Arc::new(EmbeddedStorage::new());
        let client: Arc<dyn Client> = Arc::new(StorageClient::new(store));
        let ns = "default";
        let child_key = Key::new("init-pro.io", "agents", ns, "ask-wf-0-ask");
        let wf = json!({
            "apiVersion": "argoproj.io/v1alpha1",
            "kind": "Workflow",
            "metadata": {"name": "ask-wf", "namespace": ns, "uid": "u-wf"},
            "spec": {"entrypoint": "main"},
        });
        let step = PlannedStep {
            node_name: "ask-wf[0].ask".into(),
            template_name: "tmpl".into(),
            pod_name: "ask-wf-0-ask".into(),
            template: json!({"agent": {"prompt": "p"}}),
            target: StepTarget::Agent,
        };
        let agent = build_agent(&wf, ns, "u-wf", &step);
        assert_eq!(agent["metadata"]["ownerReferences"][0]["kind"], "Workflow");

        // Case 1: the owner is NOT in the caches -> the Agent is an orphan.
        client.create(&child_key, agent.clone()).await.unwrap();
        let caches = Arc::new(Caches::new());
        assert!(caches.workflows.get(ns, "ask-wf").is_none());
        sweep(&client, &caches).await.unwrap();
        assert!(client.get(&child_key).await.unwrap().is_none());

        // Case 2: the owner IS in the workflow cache -> the Agent survives.
        client.create(&child_key, agent).await.unwrap();
        caches
            .workflows
            .apply_event(&WatchEvent::Put(Arc::new(StoredEntry {
                key: format!("/registry/argoproj.io/workflows/{ns}/ask-wf"),
                value: wf,
                create_revision: 1,
                mod_revision: 1,
                version: 1,
            })));
        assert!(caches.workflows.get(ns, "ask-wf").is_some());
        sweep(&client, &caches).await.unwrap();
        let survivor = client.get(&child_key).await.unwrap();
        assert!(survivor.is_some(), "agent must survive a live owner");
        assert_eq!(
            survivor.unwrap()["metadata"]["labels"]["workflows.argoproj.io/workflow"],
            "ask-wf"
        );
    }

    /// A2 regression: on a cold start (or right after a leader move) the
    /// owner caches have not LISTed yet. An owner that exists in storage
    /// but is absent from the empty cache must NOT be treated as deleted.
    #[tokio::test]
    async fn cold_cache_never_sweeps_live_dependents() {
        let client: Arc<dyn Client> =
            Arc::new(StorageClient::new(Arc::new(EmbeddedStorage::new())));
        let dep_key = Key::new("apps", "deployments", "default", "web");
        let rs_key = Key::new("apps", "replicasets", "default", "web-1");
        client
            .create(
                &dep_key,
                json!({"apiVersion": "apps/v1", "kind": "Deployment",
                       "metadata": {"name": "web", "namespace": "default", "uid": "u-web"}}),
            )
            .await
            .unwrap();
        client
            .create(
                &rs_key,
                json!({"apiVersion": "apps/v1", "kind": "ReplicaSet",
                       "metadata": {"name": "web-1", "namespace": "default",
                                    "ownerReferences": [{"kind": "Deployment", "name": "web",
                                                         "uid": "u-web", "controller": true}]}}),
            )
            .await
            .unwrap();

        let caches = Arc::new(Caches::new());
        // Simulate the informer's cold start: LIST in flight, cache empty.
        caches.deployments.mark_unsynced();
        sweep(&client, &caches).await.unwrap();
        assert!(
            client.get(&rs_key).await.unwrap().is_some(),
            "A2: an unsynced cache must never delete a live dependent"
        );

        // Once the LIST has landed (and the owner really is gone), the
        // backstop resumes normal service.
        caches.deployments.mark_synced();
        sweep(&client, &caches).await.unwrap();
        assert!(
            client.get(&rs_key).await.unwrap().is_none(),
            "synced caches still reap dangling dependents"
        );
    }
}
