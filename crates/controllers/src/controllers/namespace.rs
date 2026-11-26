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
//!
//! The drain table below covers EVERY namespaced kind the apiserver serves
//! (the `cli::discovery::served_schema` registry): the core/v1 set incl.
//! `events` and `serviceaccounts` (T1.3b, Q47), the apps/v1 workload set,
//! `coordination.k8s.io/leases`, the `rbac.authorization.k8s.io` namespaced
//! pair, plus the served custom groups -- Workflows + CronWorkflows (T7.3,
//! Q30), Agents (A3, Q35), LuaRouters + TrafficSplits (T8.2, Q57) and
//! Builds + ServicePipelines (T8.3, Q56), Ingresses + NetworkPolicies (Q32,
//! T4.4/Q64). Missing one of these used to orphan the kind at the terminal
//! delete: the ns object vanished while its contents survived, and a
//! surviving object pins the namespace in `Terminating` forever.
//!
//! [`served_namespaced_kinds`] cross-validates the table against the served
//! schema, so a newly served kind FAILS the build until it is drained here
//! or explicitly exempted in [`DRAIN_EXEMPT_KINDS`] with a reason.

use std::sync::Arc;

use serde_json::Value;
use storage::{Key, KeyPrefix};

use crate::client::Client;
use crate::error::ControllerError;
use crate::object::{name, resource_version};

/// `(group, resource)` kinds drained before a Namespace may finalize.
/// Group strings are the group WITHOUT the version (e.g. `argoproj.io`,
/// not `argoproj.io/v1alpha1`) -- that is how `KeyPrefix`/`Key` scope
/// storage keys. Order matters mildly: Pods drain before the workload
/// controllers that would recreate them, CronWorkflows before Workflows
/// (a live CronWorkflow re-fires children), Builds before ServicePipelines
/// (a live pipeline would recreate its child, T8.3/Q56), and leases
/// (leader election) drain last.
pub(crate) const DRAIN_KINDS: [(&str, &str); 25] = [
    ("", "pods"),
    ("", "services"),
    ("", "endpoints"),
    ("", "configmaps"),
    ("", "secrets"),
    ("", "serviceaccounts"),
    ("", "persistentvolumeclaims"),
    ("", "events"),
    ("apps", "deployments"),
    ("apps", "replicasets"),
    ("apps", "statefulsets"),
    ("apps", "daemonsets"),
    ("apps", "controllerrevisions"),
    ("argoproj.io", "cronworkflows"),
    ("argoproj.io", "workflows"),
    ("init-pro.io", "builds"),
    ("init-pro.io", "servicepipelines"),
    ("init-pro.io", "agents"),
    ("init-pro.io", "luarouters"),
    ("init-pro.io", "trafficsplits"),
    ("networking.k8s.io", "ingresses"),
    ("networking.k8s.io", "networkpolicies"),
    ("rbac.authorization.k8s.io", "roles"),
    ("rbac.authorization.k8s.io", "rolebindings"),
    ("coordination.k8s.io", "leases"),
];

/// Namespaced served kinds the drain deliberately leaves alone, each with
/// the reason it is safe to skip. Empty today: every namespaced kind the
/// apiserver serves is drained (cluster-scoped kinds are out of scope by
/// construction -- they carry no namespace). [`reconcile`] only ever
/// iterates [`DRAIN_KINDS`], so an exemption is by construction not drained;
/// this table exists so that the cross-validation test states the choice
/// explicitly instead of silently ignoring an unknown kind.
#[cfg(test)]
pub(crate) const DRAIN_EXEMPT_KINDS: [(&str, &str); 0] = [];

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

/// Native kinds `cli::discovery::served_schema` registers on top of
/// `SchemaRegistry::with_core_v1()` (the core/v1 + apps/v1 +
/// coordination.k8s.io rows). Mirrored rather than imported: the `cli` crate
/// depends on `controllers`, so the served registry cannot be a dependency
/// here. `(group, version, kind, resource, scope)`.
#[cfg(test)]
const SERVED_NATIVE: [(&str, &str, &str, &str, api::Scope); 9] = [
    ("", "v1", "Endpoints", "endpoints", api::Scope::Namespaced),
    (
        "",
        "v1",
        "PersistentVolumeClaim",
        "persistentvolumeclaims",
        api::Scope::Namespaced,
    ),
    (
        "",
        "v1",
        "ServiceAccount",
        "serviceaccounts",
        api::Scope::Namespaced,
    ),
    (
        "apps",
        "v1",
        "Deployment",
        "deployments",
        api::Scope::Namespaced,
    ),
    (
        "apps",
        "v1",
        "ReplicaSet",
        "replicasets",
        api::Scope::Namespaced,
    ),
    (
        "apps",
        "v1",
        "StatefulSet",
        "statefulsets",
        api::Scope::Namespaced,
    ),
    (
        "apps",
        "v1",
        "ControllerRevision",
        "controllerrevisions",
        api::Scope::Namespaced,
    ),
    (
        "apps",
        "v1",
        "DaemonSet",
        "daemonsets",
        api::Scope::Namespaced,
    ),
    (
        "coordination.k8s.io",
        "v1",
        "Lease",
        "leases",
        api::Scope::Namespaced,
    ),
];

#[cfg(test)]
mod tests {
    use super::*;
    use api::{self, SchemaRegistry, Scope};
    use serde_json::json;

    /// The served schema, assembled exactly like
    /// `cli::discovery::served_schema(true)` (auth armed: the RBAC group and
    /// core ServiceAccount are served). The custom groups come from the
    /// `api::*::register` calls -- the same ones `cli` uses -- so a new CRD
    /// kind lands here automatically; a new NATIVE kind needs a row in
    /// [`SERVED_NATIVE`] (and the two-way assertions below catch the typo).
    fn served_registry() -> SchemaRegistry {
        let mut reg = SchemaRegistry::with_core_v1();
        for (group, version, kind, resource, scope) in SERVED_NATIVE {
            reg.register(
                group,
                version,
                kind,
                resource,
                &format!("{kind}List"),
                scope,
            );
        }
        api::argoproj::register(&mut reg);
        api::initpro::register(&mut reg);
        api::agent::register(&mut reg);
        api::traffic::register(&mut reg);
        api::pipeline::register(&mut reg);
        api::build::register(&mut reg);
        api::networking::register(&mut reg);
        api::rbac::register(&mut reg);
        reg
    }

    /// Every namespaced `(group, resource)` the served schema carries
    /// (cluster-scoped kinds carry no namespace, so the drain ignores them).
    fn served_namespaced_kinds() -> Vec<(String, String)> {
        let mut kinds: Vec<(String, String)> = served_registry()
            .iter()
            .filter(|(_, info)| info.scope.is_namespaced())
            .map(|(gvk, info)| (gvk.group.clone(), info.resource.clone()))
            .collect();
        kinds.sort();
        kinds
    }

    /// Served kinds that are neither drained nor exempted -- the exact
    /// predicate the drain coverage is judged by (pure, so the drift test
    /// below can drive it with a synthetic kind).
    fn undrained_kinds(
        served: &[(String, String)],
        drained: &[(&str, &str)],
        exempt: &[(&str, &str)],
    ) -> Vec<String> {
        served
            .iter()
            .filter(|(group, resource)| {
                let kind = (group.as_str(), resource.as_str());
                !drained.contains(&kind) && !exempt.contains(&kind)
            })
            .map(|(group, resource)| format!("{group}/{resource}"))
            .collect()
    }

    /// B5: every namespaced kind the apiserver serves is drained (or
    /// explicitly exempted). A missing row is exactly the bug that pinned a
    /// namespace in `Terminating` forever.
    #[test]
    fn drain_table_covers_every_served_namespaced_kind() {
        let missing = undrained_kinds(
            &served_namespaced_kinds(),
            &DRAIN_KINDS,
            &DRAIN_EXEMPT_KINDS,
        );
        assert!(
            missing.is_empty(),
            "served but neither drained nor exempted: {missing:?}"
        );
    }

    /// The inverse guard: a drained kind the apiserver does not serve is a
    /// typo or a stale row (it costs one list per namespace teardown).
    #[test]
    fn every_drained_kind_is_served() {
        let served = served_namespaced_kinds();
        for (group, resource) in DRAIN_KINDS {
            assert!(
                served.iter().any(|(g, r)| g == group && r == resource),
                "DRAIN_KINDS row {group}/{resource} is not served"
            );
        }
        assert_eq!(DRAIN_KINDS.len(), 25);
    }

    /// The cross-validation is load-bearing: serving a new namespaced kind
    /// without draining it (or exempting it with a reason) FAILS the check.
    #[test]
    fn a_newly_served_kind_is_reported_until_registered_or_exempted() {
        let mut reg = served_registry();
        reg.register(
            "example.io",
            "v1",
            "Widget",
            "widgets",
            "WidgetList",
            Scope::Namespaced,
        );
        let mut served: Vec<(String, String)> = reg
            .iter()
            .filter(|(_, info)| info.scope.is_namespaced())
            .map(|(gvk, info)| (gvk.group.clone(), info.resource.clone()))
            .collect();
        served.sort();

        assert_eq!(
            undrained_kinds(&served, &DRAIN_KINDS, &DRAIN_EXEMPT_KINDS),
            vec!["example.io/widgets".to_string()],
            "the new kind must be reported"
        );
        // Registering it (or exempting it, with a reason) silences the guard.
        let mut drained = DRAIN_KINDS.to_vec();
        drained.push(("example.io", "widgets"));
        assert!(undrained_kinds(&served, &drained, &DRAIN_EXEMPT_KINDS).is_empty());
        assert!(undrained_kinds(&served, &DRAIN_KINDS, &[("example.io", "widgets")]).is_empty());
        // A CLUSTER-scoped kind needs no drain row at all.
        let mut reg = served_registry();
        reg.register(
            "example.io",
            "v1",
            "ClusterWidget",
            "clusterwidgets",
            "ClusterWidgetList",
            Scope::Cluster,
        );
        let mut served: Vec<(String, String)> = reg
            .iter()
            .filter(|(_, info)| info.scope.is_namespaced())
            .map(|(gvk, info)| (gvk.group.clone(), info.resource.clone()))
            .collect();
        served.sort();
        assert!(undrained_kinds(&served, &DRAIN_KINDS, &DRAIN_EXEMPT_KINDS).is_empty());
    }

    #[test]
    fn drain_kind_table_covers_the_controller_footprint() {
        for kind in [
            ("", "pods"),
            ("", "services"),
            ("", "endpoints"),
            ("", "configmaps"),
            ("", "secrets"),
            ("", "serviceaccounts"),
            ("", "persistentvolumeclaims"),
            ("", "events"),
            ("apps", "deployments"),
            ("apps", "replicasets"),
            ("apps", "statefulsets"),
            ("apps", "daemonsets"),
            ("apps", "controllerrevisions"),
            ("argoproj.io", "cronworkflows"),
            ("argoproj.io", "workflows"),
            ("init-pro.io", "agents"),
            ("init-pro.io", "builds"),
            ("init-pro.io", "servicepipelines"),
            ("init-pro.io", "luarouters"),
            ("init-pro.io", "trafficsplits"),
            ("networking.k8s.io", "ingresses"),
            ("networking.k8s.io", "networkpolicies"),
            ("rbac.authorization.k8s.io", "roles"),
            ("rbac.authorization.k8s.io", "rolebindings"),
            ("coordination.k8s.io", "leases"),
        ] {
            assert!(DRAIN_KINDS.contains(&kind), "missing {kind:?}");
        }
        // Pods drain before the workload controllers that would recreate
        // them; every served custom-group kind (T7.3 Q30, A3 Q35, Q32) is
        // in the table so the terminal delete cannot orphan it; leases
        // (leader election) drain last.
        let pods = DRAIN_KINDS.iter().position(|k| k == &("", "pods")).unwrap();
        let deps = DRAIN_KINDS
            .iter()
            .position(|k| k == &("apps", "deployments"))
            .unwrap();
        assert!(pods < deps, "pods must drain before deployments");
        let cwfs = DRAIN_KINDS
            .iter()
            .position(|k| k == &("argoproj.io", "cronworkflows"))
            .unwrap();
        let wfs = DRAIN_KINDS
            .iter()
            .position(|k| k == &("argoproj.io", "workflows"))
            .unwrap();
        assert!(cwfs < wfs, "cronworkflows must drain before workflows");
        assert_eq!(
            DRAIN_KINDS.last(),
            Some(&("coordination.k8s.io", "leases")),
            "leases drain last"
        );
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
