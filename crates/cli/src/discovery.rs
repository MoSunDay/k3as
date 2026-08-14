//! API discovery wiring (TODO **T1.1**, S7 skeleton).
//!
//! Builds the [`SchemaRegistry`] served by `init-pro server`: core/v1
//! pass-through (now including `endpoints`, needed by the T3.1a endpoints
//! controller), the `apps/v1` group (T3.1a Deployment/ReplicaSet controllers;
//! StatefulSet/DaemonSet are registered schema-only until T3.1b),
//! `coordination.k8s.io/v1` (Lease — leader election, decision **Q18**), and
//! the `init-pro.io/v1` group. The actual HTTP serving is **T1.2**; this
//! module hands T1.2 a byte-correct registry today and is exercised at startup.

use api::{api_group_list, core_api_versions, initpro, SchemaRegistry};

/// The served schema registry: core/v1 + apps/v1 + coordination.k8s.io/v1
/// native types + init-pro.io/v1 CRDs.
pub fn served_schema() -> SchemaRegistry {
    let mut reg = SchemaRegistry::with_core_v1();
    // Endpoints is core/v1: the endpoints controller (T3.1a) reflects Service
    // selector membership into it.
    reg.register_native::<k8s_openapi::api::core::v1::Endpoints>();
    // apps/v1: Deployment + ReplicaSet are driven by the T3.1a controllers;
    // StatefulSet/DaemonSet are SCHEMA-ONLY for now (inert — no controller
    // until T3.1b), registered so the group/version contract is stable.
    reg.register_native::<k8s_openapi::api::apps::v1::Deployment>();
    reg.register_native::<k8s_openapi::api::apps::v1::ReplicaSet>();
    reg.register_native::<k8s_openapi::api::apps::v1::StatefulSet>();
    reg.register_native::<k8s_openapi::api::apps::v1::DaemonSet>();
    // coordination.k8s.io/v1: Lease backs controller-manager leader election
    // (decision Q18: Lease object + CAS, not etcd leases).
    reg.register_native::<k8s_openapi::api::coordination::v1::Lease>();
    initpro::register(&mut reg);
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

    #[test]
    fn served_schema_has_core_and_initpro() {
        let reg = served_schema();
        assert!(reg.len() >= 8); // 8 core/v1 + apps/v1 + coordination + LuaRouter
        let summary = served_groups_summary(&reg, "127.0.0.1:6443");
        assert!(summary.contains("(core)"));
        assert!(summary.contains("init-pro.io"));
        assert!(summary.contains("v1"));
        // T3.1a: apps/v1 + coordination.k8s.io/v1 must be served groups.
        assert!(summary.contains("apps"));
        assert!(summary.contains("coordination.k8s.io"));
        // ... with the exact resource index the controllers depend on.
        let apps = api_resource_list(&reg, "apps", "v1").expect("apps/v1 served");
        let names: Vec<&str> = apps.resources.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(
            names,
            ["daemonsets", "deployments", "replicasets", "statefulsets"]
        );
        let coordination =
            api_resource_list(&reg, "coordination.k8s.io", "v1").expect("coordination served");
        assert!(coordination.resources.iter().any(|r| r.name == "leases"));
        // core/v1 now includes endpoints (T3.1a endpoints controller).
        let core = api_resource_list(&reg, "", "v1").expect("core/v1 served");
        assert!(core.resources.iter().any(|r| r.name == "endpoints"));
    }
}
