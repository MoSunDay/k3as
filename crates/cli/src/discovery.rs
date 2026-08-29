//! API discovery wiring (TODO **T1.1**, S7 skeleton).
//!
//! Builds the [`SchemaRegistry`] served by `init-pro server`: core/v1
//! pass-through (now including `endpoints` for the T3.1a endpoints
//! controller and `persistentvolumeclaims` for the T3.1b StatefulSet
//! controller), the `apps/v1` group (T3.1a Deployment/ReplicaSet
//! controllers, T3.1b StatefulSet + its `controllerrevisions` history;
//! DaemonSet stays schema-only until its T3.1b slice),
//! `coordination.k8s.io/v1` (Lease — leader election, decision **Q18**), the
//! `init-pro.io/v1` group, and `argoproj.io/v1alpha1` (T7.3 Scope A:
//! Workflow + CronWorkflow, schema-only CRUD — no workflow controller yet). The actual HTTP serving is **T1.2**; this
//! module hands T1.2 a byte-correct registry today and is exercised at startup.

use api::{
    agent, api_group_list, argoproj, core_api_versions, initpro, networking, SchemaRegistry,
};

/// The served schema registry: core/v1 + apps/v1 + coordination.k8s.io/v1
/// native types + init-pro.io/v1 CRDs.
pub fn served_schema() -> SchemaRegistry {
    let mut reg = SchemaRegistry::with_core_v1();
    // Endpoints is core/v1: the endpoints controller (T3.1a) reflects Service
    // selector membership into it.
    reg.register_native::<k8s_openapi::api::core::v1::Endpoints>();
    // PersistentVolumeClaim is core/v1: created by the T3.1b StatefulSet
    // controller from volumeClaimTemplates (lazy storage, decision Q22 —
    // no binder until T6.2).
    reg.register_native::<k8s_openapi::api::core::v1::PersistentVolumeClaim>();
    // apps/v1: Deployment + ReplicaSet are driven by the T3.1a controllers;
    // StatefulSet (T3.1b) records its pod-template history as
    // ControllerRevision objects; DaemonSet is SCHEMA-ONLY for now (inert —
    // no controller until its T3.1b slice), registered so the group/version
    // contract is stable.
    reg.register_native::<k8s_openapi::api::apps::v1::Deployment>();
    reg.register_native::<k8s_openapi::api::apps::v1::ReplicaSet>();
    reg.register_native::<k8s_openapi::api::apps::v1::StatefulSet>();
    reg.register_native::<k8s_openapi::api::apps::v1::ControllerRevision>();
    reg.register_native::<k8s_openapi::api::apps::v1::DaemonSet>();
    // coordination.k8s.io/v1: Lease backs controller-manager leader election
    // (decision Q18: Lease object + CAS, not etcd leases).
    reg.register_native::<k8s_openapi::api::coordination::v1::Lease>();
    // argoproj.io/v1alpha1 (T7.3 Scope A): Workflow + CronWorkflow are
    // served as first-class built-ins — full CRUD/watch/SSA via the generic
    // routes, schema-only (the workflow controller is a later scope, Q30).
    argoproj::register(&mut reg);
    initpro::register(&mut reg);
    // init-pro.io/v1 Agent (TODO **A3**, Q35): a schedulable opencoder session,
    // served through the same generic CRUD/watch routes as any CRD.
    agent::register(&mut reg);
    // networking.k8s.io/v1 (Q32): Ingress + IngressClass feed the built-in
    // openrusty gateway (generic CRUD/watch routes; the gateway reflects
    // the same objects through its storage watchers).
    networking::register(&mut reg);
    reg
}

/// One-line summary of served API groups (for startup logging).
pub fn served_groups_summary(reg: &SchemaRegistry, server_addr: &str) -> String {
    let core = core_api_versions(reg, server_addr);
    let groups = api_group_list(reg);
    let mut names: Vec<&str> = groups.groups.iter().map(|g| g.name.as_str()).collect();
    names.insert(0, "(core)");
    format!(
        "serving core versions [{}] + {} group(s): {}",
        core.versions.join(", "),
        groups.groups.len(),
        names.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use api::api_resource_list;
    use api::GroupVersionKind;

    #[test]
    fn served_schema_has_core_and_initpro() {
        let reg = served_schema();
        assert!(reg.len() >= 21); // 9 core/v1 + 5 apps/v1 + coordination + 2 argoproj + 2 networking + 1 init-pro.io + agents
        let summary = served_groups_summary(&reg, "127.0.0.1:6443");
        assert!(summary.contains("(core)"));
        assert!(summary.contains("init-pro.io"));
        assert!(summary.contains("v1"));
        // T3.1a: apps/v1 + coordination.k8s.io/v1 must be served groups.
        assert!(summary.contains("apps"));
        assert!(summary.contains("coordination.k8s.io"));
        // ... with the exact resource index the controllers depend on
        // (T3.1b: StatefulSet is live and owns ControllerRevision history).
        let apps = api_resource_list(&reg, "apps", "v1").expect("apps/v1 served");
        let names: Vec<&str> = apps.resources.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "controllerrevisions",
                "daemonsets",
                "deployments",
                "replicasets",
                "statefulsets"
            ]
        );
        let coordination =
            api_resource_list(&reg, "coordination.k8s.io", "v1").expect("coordination served");
        assert!(coordination.resources.iter().any(|r| r.name == "leases"));
        // T7.3 Scope A: argoproj.io/v1alpha1 serves Workflow + CronWorkflow
        // with their kubectl short names (wf / cwf, cronwf).
        let argo = api_resource_list(&reg, "argoproj.io", "v1alpha1").expect("argoproj served");
        let wf = argo
            .resources
            .iter()
            .find(|r| r.name == "workflows")
            .expect("workflows served");
        assert_eq!(wf.short_names, Some(vec!["wf".to_string()]));
        let cwf = argo
            .resources
            .iter()
            .find(|r| r.name == "cronworkflows")
            .expect("cronworkflows served");
        assert_eq!(
            cwf.short_names,
            Some(vec!["cwf".to_string(), "cronwf".to_string()])
        );
        // core/v1 now includes endpoints (T3.1a) and persistentvolumeclaims
        // (T3.1b StatefulSet claims, lazy storage Q22).
        // Q32: networking.k8s.io/v1 serves ingresses (short name `ing`,
        // namespaced) + ingressclasses (cluster-scoped).
        assert!(summary.contains("networking.k8s.io"));
        let net = api_resource_list(&reg, "networking.k8s.io", "v1")
            .expect("networking.k8s.io/v1 served");
        let ing = net
            .resources
            .iter()
            .find(|r| r.name == "ingresses")
            .expect("ingresses served");
        assert_eq!(ing.kind, "Ingress");
        assert_eq!(ing.short_names, Some(vec!["ing".to_string()]));
        assert!(ing.namespaced);
        let classes = net
            .resources
            .iter()
            .find(|r| r.name == "ingressclasses")
            .expect("ingressclasses served");
        assert_eq!(classes.kind, "IngressClass");
        assert!(!classes.namespaced);
        let core = api_resource_list(&reg, "", "v1").expect("core/v1 served");
        assert!(core.resources.iter().any(|r| r.name == "endpoints"));
        assert!(core
            .resources
            .iter()
            .any(|r| r.name == "persistentvolumeclaims"));
    }

    #[test]
    fn served_schema_has_agents() {
        let reg = served_schema();
        // TODO **A3** (Q35): `agents` rides the generic routes as-is, so the
        // GVK<->GVR join must exist with the exact kubectl-facing metadata.
        let info = reg
            .get(&GroupVersionKind::gvk("init-pro.io", "v1", "Agent"))
            .expect("Agent GVK resolved");
        assert_eq!(info.kind, "Agent");
        assert_eq!(info.resource, "agents");
        assert_eq!(info.list_kind, "AgentList");
        assert!(info.scope.is_namespaced());
        assert_eq!(info.short_names, vec!["ag".to_string()]);
        // ... and discovery projects it under the init-pro.io/v1 group.
        let group = api_resource_list(&reg, "init-pro.io", "v1").expect("init-pro.io/v1 served");
        let agents = group
            .resources
            .iter()
            .find(|r| r.name == "agents")
            .expect("agents served");
        assert_eq!(agents.kind, "Agent");
        assert_eq!(agents.short_names, Some(vec!["ag".to_string()]));
        assert!(agents.namespaced);
    }
}
