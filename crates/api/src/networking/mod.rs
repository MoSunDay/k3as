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
//! Scope: [`Ingress`] + [`IngressClass`] (GA v1 shapes) with list wrappers,
//! plus [`NetworkPolicy`] + its semantics/validation module (TODO **T4.4**,
//! decision **Q64**; consumed by the node-side enforcement keeper).
//! `IPAddress`/`ServiceCIDR` are out of scope (no consumer).
//! Minimal-fidelity notes (documented, upstream-faithful but reduced):
//! - `IngressLoadBalancerIngress.ports` is not carried (no LB status writer).
//! - `IngressClassSpec.parameters` is not carried (no external controller).
//! - `IngressTLS` is parsed but unused by the gateway (Q32: h2c only; TLS
//!   termination is a documented deviation from the Lua-era T5.4).

pub mod ingress;
pub mod ingress_class;
pub mod network_policy;
pub mod network_policy_rules;

use crate::schema::{SchemaRegistry, Scope};

/// API group for Ingress / IngressClass.
pub const GROUP: &str = "networking.k8s.io";
/// Served version (upstream GA group version).
pub const VERSION: &str = "v1";
/// `group/version` wire string.
pub const API_VERSION: &str = "networking.k8s.io/v1";

/// Register the `networking.k8s.io/v1` kinds into a [`SchemaRegistry`]:
/// `ingresses` (namespaced, short name `ing`), `ingressclasses`
/// (cluster-scoped), and `networkpolicies` (namespaced, short name
/// `netpol`, TODO **T4.4** / decision **Q64** — consumed by the node-side
/// enforcement keeper on the agent). Registration is generic CRUD/watch
/// via the apiserver routes — the gateway consumes the same objects
/// through its reflectors.
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
    reg.register_with_short_names(
        GROUP,
        VERSION,
        "NetworkPolicy",
        "networkpolicies",
        "NetworkPolicyList",
        Scope::Namespaced,
        &["netpol"],
    );
}

pub use ingress::{
    HTTPIngressPath, HTTPIngressRuleValue, Ingress, IngressBackend, IngressList,
    IngressLoadBalancerIngress, IngressLoadBalancerStatus, IngressRule, IngressServiceBackend,
    IngressSpec, IngressStatus, IngressTLS, ServiceBackendPort, TypedLocalObjectReference,
};
pub use ingress_class::{IngressClass, IngressClassList, IngressClassSpec};
pub use network_policy::{
    IPBlock, IntOrString, LabelSelector, LabelSelectorRequirement, NetworkPolicy,
    NetworkPolicyCondition, NetworkPolicyEgressRule, NetworkPolicyIngressRule, NetworkPolicyList,
    NetworkPolicyPeer, NetworkPolicyPort, NetworkPolicySpec, NetworkPolicyStatus,
};
pub use network_policy_rules::{
    effective_policy_types, ipv4_cidr_range, label_selector_matches, validate_network_policy,
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::serde_ext::from_json;

    /// Wire parity for `NetworkPolicy` (T4.4, Q64): camelCase json tags,
    /// `from`/`to` peer keys, omitempty round-trip, `status` absent.
    #[test]
    fn network_policy_wire_format_is_k8s_faithful() {
        let doc = serde_json::json!({
            "apiVersion": "networking.k8s.io/v1",
            "kind": "NetworkPolicy",
            "metadata": {"name": "deny-all", "namespace": "default"},
            "spec": {
                "podSelector": {"matchLabels": {"app": "victim"}},
                "policyTypes": ["Ingress", "Egress"],
                "ingress": [{
                    "from": [
                        {"podSelector": {"matchLabels": {"app": "friend"}}},
                        {"namespaceSelector": {"matchLabels": {"tier": "edge"}}},
                        {"ipBlock": {"cidr": "10.42.0.0/16", "except": ["10.42.1.0/24"]}}
                    ],
                    "ports": [{"protocol": "TCP", "port": 8080}]
                }],
                "egress": [{"to": [{"ipBlock": {"cidr": "0.0.0.0/0"}}]}]
            }
        });
        let np: NetworkPolicy = from_json(&serde_json::to_vec(&doc).unwrap()).expect("parse");
        assert_eq!(np.types.api_version, API_VERSION);
        assert_eq!(np.types.kind, "NetworkPolicy");
        let spec = np.spec.as_ref().expect("spec kept");
        assert_eq!(
            spec.pod_selector.match_labels.as_ref().unwrap()["app"],
            "victim"
        );
        assert_eq!(
            spec.policy_types.as_deref(),
            Some(["Ingress".to_string(), "Egress".to_string()].as_slice())
        );
        let peer = &spec.ingress.as_ref().unwrap()[0].from.as_ref().unwrap()[2];
        assert_eq!(peer.ip_block.as_ref().unwrap().cidr, "10.42.0.0/16");
        // Round-trip: only populated fields serialize; status stays absent.
        let wire: serde_json::Value =
            serde_json::from_slice(&crate::serde_ext::to_json(&np).unwrap()).unwrap();
        assert!(wire.get("status").is_none(), "status is omitempty");
        let back: NetworkPolicy = from_json(&serde_json::to_vec(&wire).unwrap()).unwrap();
        assert_eq!(back, np);
    }

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
        assert_eq!(spec.ingress_class_name.as_deref(), Some("init-pro"));
        let rule = &spec.rules.as_ref().expect("rules kept")[0];
        let path = &rule.http.as_ref().expect("http kept").paths[0];
        assert_eq!(path.path_type, "Prefix");
        assert_eq!(path.path, "/v1");
        let backend = &path.backend.service.as_ref().expect("svc backend");
        assert_eq!(backend.name, "api");
        assert_eq!(backend.port.as_ref().unwrap().name, "http");

        // Round-trip: only populated fields are serialized (omitempty).
        let wire: serde_json::Value =
            serde_json::from_slice(&crate::serde_ext::to_json(&ing).unwrap())
                .expect("serialize to Value");
        assert!(wire.get("spec").is_some());
        assert!(wire.get("status").is_none(), "status is omitempty");
        assert!(wire["spec"].get("tls").is_some());
        let back: Ingress = from_json(&serde_json::to_vec(&wire).unwrap()).expect("round-trip");
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

    /// Registry wiring: all three kinds are discoverable with kubectl short
    /// names (T4.4 added `networkpolicies`/`netpol`, Q64).
    #[test]
    fn registry_exposes_ingresses_and_classes() {
        let mut reg = SchemaRegistry::with_core_v1();
        register(&mut reg);
        let list = crate::api_resource_list(&reg, GROUP, VERSION).expect("group served");
        let mut names: Vec<&str> = list.resources.iter().map(|r| r.name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, ["ingressclasses", "ingresses", "networkpolicies"]);
        let ing = list
            .resources
            .iter()
            .find(|r| r.name == "ingresses")
            .expect("ingresses served");
        assert_eq!(ing.kind, "Ingress");
        assert!(ing.namespaced, "ingresses are namespaced");
        assert_eq!(ing.short_names, Some(vec!["ing".to_string()]));
        let npol = list
            .resources
            .iter()
            .find(|r| r.name == "networkpolicies")
            .expect("networkpolicies served");
        assert_eq!(npol.kind, "NetworkPolicy");
        assert!(npol.namespaced, "networkpolicies are namespaced");
        assert_eq!(npol.short_names, Some(vec!["netpol".to_string()]));
    }
}
