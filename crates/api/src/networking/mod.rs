//! `networking.k8s.io/v1` API group types (workstream A / decision **Q32**).
//!
//! 1:1 typed port of the upstream `networking/v1` resource model that the
//! Ingress gateway consumes. Field-level SSOT is the Go json tags in
//! `k8s.io/api/networking/v1` (`types.go`); wire format is JSON-only
//! (decision **Q10**). The old Lua-era router read these shapes via
//! `k8s-openapi`; after the openrusty rewrite (Q32) the gateway owns its
//! model in-tree (`api` crate), dropping the `k8s-openapi`/`kube-core`
//! networking deps from the data plane.
//!
//! Scope: [`Ingress`] + [`IngressClass`] (GA v1 shapes) with list wrappers.
//! `NetworkPolicy`/`IPAddress`/`ServiceCIDR` are out of scope (no consumer).
//! Minimal-fidelity notes (documented, upstream-faithful but reduced):
//! - `IngressLoadBalancerIngress.ports` is not carried (no LB status writer).
//! - `IngressClassSpec.parameters` is not carried (no external controller).
//! - `IngressTLS` is parsed but unused by the gateway (Q32: h2c only; TLS
//!   termination is a documented deviation from the Lua-era T5.4).

pub mod ingress;
pub mod ingress_class;

use crate::schema::{SchemaRegistry, Scope};

/// API group for Ingress / IngressClass.
pub const GROUP: &str = "networking.k8s.io";
/// Served version (upstream GA group version).
pub const VERSION: &str = "v1";
/// `group/version` wire string.
pub const API_VERSION: &str = "networking.k8s.io/v1";

/// Register the `networking.k8s.io/v1` kinds into a [`SchemaRegistry`]:
/// `ingresses` (namespaced, short name `ing`) and `ingressclasses`
/// (cluster-scoped). Registration is generic CRUD/watch via the apiserver
/// routes — the gateway consumes the same objects through its reflectors.
pub fn register(reg: &mut SchemaRegistry) {
    reg.register_with_short_names(
        GROUP,
        VERSION,
        "Ingress",
        "ingresses",
        "IngressList",
        Scope::Namespaced,
        &["ing"],
    );
    reg.register_with_short_names(
        GROUP,
        VERSION,
        "IngressClass",
        "ingressclasses",
        "IngressClassList",
        Scope::Cluster,
        &[],
    );
}

pub use ingress::{
    HTTPIngressPath, HTTPIngressRuleValue, Ingress, IngressBackend, IngressList, IngressLoadBalancerIngress,
    IngressLoadBalancerStatus, IngressRule, IngressServiceBackend, IngressSpec, IngressStatus, IngressTLS,
    ServiceBackendPort, TypedLocalObjectReference,
};
pub use ingress_class::{IngressClass, IngressClassList, IngressClassSpec};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::serde_ext::from_json;

    /// Wire parity with upstream `networking/v1` json tags: camelCase keys,
    /// `typeMeta` flattened to `apiVersion`/`kind`, omitempty round-trips.
    #[test]
    fn ingress_wire_format_is_k8s_faithful() {
        let doc = serde_json::json!({
            "apiVersion": "networking.k8s.io/v1",
            "kind": "Ingress",
            "metadata": {"name": "edge", "namespace": "default"},
            "spec": {
                "ingressClassName": "init-pro",
                "defaultBackend": {"service": {"name": "fallback", "port": {"number": 80}}},
                "rules": [{
                    "host": "api.example.com",
                    "http": {"paths": [{
                        "path": "/v1",
                        "pathType": "Prefix",
                        "backend": {"service": {"name": "api", "port": {"name": "http"}}}
                    }]}
                }],
                "tls": [{"hosts": ["api.example.com"], "secretName": "edge-tls"}]
            }
        });
        let ing: Ingress = from_json(&serde_json::to_vec(&doc).unwrap()).expect("parse Ingress");
        assert_eq!(ing.types.api_version, API_VERSION);
        assert_eq!(ing.types.kind, "Ingress");
        let spec = ing.spec.as_ref().expect("spec kept");
        assert_eq!(
            spec.ingress_class_name.as_deref(),
            Some("init-pro")
        );
        let rule = &spec.rules.as_ref().expect("rules kept")[0];
        let path = &rule.http.as_ref().expect("http kept").paths[0];
        assert_eq!(path.path_type, "Prefix");
        assert_eq!(path.path, "/v1");
        let backend = &path.backend.service.as_ref().expect("svc backend");
        assert_eq!(backend.name, "api");
        assert_eq!(backend.port.as_ref().unwrap().name, "http");

        // Round-trip: only populated fields are serialized (omitempty).
        let wire: serde_json::Value = serde_json::from_slice(&crate::serde_ext::to_json(&ing).unwrap())
            .expect("serialize to Value");
        assert!(wire.get("spec").is_some());
        assert!(wire.get("status").is_none(), "status is omitempty");
        assert!(wire["spec"].get("tls").is_some());
        let back: Ingress =
            from_json(&serde_json::to_vec(&wire).unwrap()).expect("round-trip");
        assert_eq!(back, ing);
    }

    /// `IngressClass` is cluster-scoped and carries `spec.controller`.
    #[test]
    fn ingress_class_wire_format() {
        let cls = IngressClass {
            types: Default::default(),
            metadata: Default::default(),
            spec: Some(IngressClassSpec {
                controller: Some("init-pro.io/ingress".into()),
            }),
        };
        let wire: serde_json::Value =
            serde_json::from_slice(&crate::serde_ext::to_json(&cls).unwrap()).expect("serialize");
        assert_eq!(wire["spec"]["controller"], "init-pro.io/ingress");
        assert!(wire.get("status").is_none());
        let back: IngressClass =
            from_json(&serde_json::to_vec(&wire).unwrap()).expect("round-trip");
        assert_eq!(back, cls);
    }

    /// Registry wiring: both kinds are discoverable with kubectl short names.
    #[test]
    fn registry_exposes_ingresses_and_classes() {
        let mut reg = SchemaRegistry::with_core_v1();
        register(&mut reg);
        let list = crate::api_resource_list(&reg, GROUP, VERSION).expect("group served");
        let mut names: Vec<&str> = list.resources.iter().map(|r| r.name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, ["ingressclasses", "ingresses"]);
        let ing = list
            .resources
            .iter()
            .find(|r| r.name == "ingresses")
            .expect("ingresses served");
        assert_eq!(ing.kind, "Ingress");
        assert!(ing.namespaced, "ingresses are namespaced");
        assert_eq!(ing.short_names, Some(vec!["ing".to_string()]));
    }
}
