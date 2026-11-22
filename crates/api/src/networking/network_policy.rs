//! `networking.k8s.io/v1` `NetworkPolicy` (TODO **T4.4**, decision **Q64**).
//!
//! 1:1 typed port of upstream `k8s.io/api/networking/v1` `types.go`
//! (`NetworkPolicy`, `NetworkPolicySpec`, `NetworkPolicyIngressRule`,
//! `NetworkPolicyEgressRule`, `NetworkPolicyPort`, `NetworkPolicyPeer`,
//! `IPBlock`, `NetworkPolicyStatus`), plus the shared `LabelSelector`
//! shape re-declared in-tree so the node-side enforcement plane needs no
//! `k8s-openapi` dependency (the [`crate::networking`] Q32 doctrine).
//! Wire format is JSON-only (decision **Q10**).
//!
//! Supported-subset reductions (Q64, locked; enforced by admission in
//! [`super::network_policy_rules`]):
//! - `protocol` is `TCP`/`UDP` only — `SCTP` is validate-rejected.
//! - `port` is numeric only — named ports are validate-rejected.
//! - `ipBlock` is IN scope (IPv4), `except` optional.
//! - `NetworkPolicyPeer` combinations follow upstream (podSelector /
//!   namespaceSelector / ipBlock; at least one required; ipBlock is
//!   exclusive of the two selectors).

use kube_core::metadata::{ObjectMeta, TypeMeta};
use serde::{Deserialize, Serialize};

/// NetworkPolicy describes what network traffic is allowed for a set of pods.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct NetworkPolicy {
    #[serde(flatten)]
    pub types: TypeMeta,
    #[serde(default)]
    pub metadata: ObjectMeta,
    /// Spec represents the specification of the desired behavior.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spec: Option<NetworkPolicySpec>,
    /// Status is the current state of the policy (minimal wire compat:
    /// upstream conditions only, never written by init-pro).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<NetworkPolicyStatus>,
}

/// NetworkPolicyList is a collection of NetworkPolicy objects.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct NetworkPolicyList {
    #[serde(flatten)]
    pub types: TypeMeta,
    #[serde(default)]
    pub metadata: kube_core::metadata::ListMeta,
    #[serde(default)]
    pub items: Vec<NetworkPolicy>,
}

/// NetworkPolicySpec provides the specification of a NetworkPolicy.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct NetworkPolicySpec {
    /// Selects the pods to which this NetworkPolicy object applies. An
    /// EMPTY selector (upstream semantics, unlike the controllers'
    /// select-nothing posture) matches all pods in the policy's namespace.
    #[serde(default)]
    pub pod_selector: LabelSelector,
    /// Ingress rules (absent/empty + `policyTypes: [Ingress]` = deny all).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ingress: Option<Vec<NetworkPolicyIngressRule>>,
    /// Egress rules (absent/empty + `policyTypes: [Egress]` = deny all).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub egress: Option<Vec<NetworkPolicyEgressRule>>,
    /// Whether this policy applies to ingress, egress, or both. Upstream
    /// default when unset: `[Ingress]`, plus `Egress` when any egress
    /// rule is present (see `network_policy_rules::effective_policy_types`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy_types: Option<Vec<String>>,
}

/// NetworkPolicyIngressRule: allowed inbound traffic (`from` OR-list with
/// `ports`; empty `from` = all sources, empty `ports` = all ports).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct NetworkPolicyIngressRule {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ports: Option<Vec<NetworkPolicyPort>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<Vec<NetworkPolicyPeer>>,
}

/// NetworkPolicyEgressRule: allowed outbound traffic (`to` OR-list with
/// `ports`), isomorphic to ingress on the source side.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct NetworkPolicyEgressRule {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ports: Option<Vec<NetworkPolicyPort>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<Vec<NetworkPolicyPeer>>,
}

/// NetworkPolicyPort: one allowed protocol/port pair. Supported subset
/// (Q64): `protocol` TCP/UDP (default TCP), `port` numeric 1-65535 only.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct NetworkPolicyPort {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<IntOrString>,
}

/// IntOrString for `port`: untagged wire form (JSON number or name).
/// Named (string) ports are OUT of the supported subset (Q64) and are
/// validate-rejected — the shape stays on the wire for parity.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(untagged)]
pub enum IntOrString {
    Int(i32),
    Str(String),
}

/// NetworkPolicyPeer: one traffic source/destination. At least one field
/// must be present (upstream validation); `ipBlock` is exclusive of the
/// two selectors.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct NetworkPolicyPeer {
    /// Pods matching this selector in the policy's OWN namespace (an empty
    /// selector selects every pod there — upstream semantics).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pod_selector: Option<LabelSelector>,
    /// Namespaces matching this selector (empty selects all namespaces).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace_selector: Option<LabelSelector>,
    /// IP block (IPv4 CIDR, optional `except` holes), exclusive of both
    /// selectors.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ip_block: Option<IPBlock>,
}

/// IPBlock describes a CIDR (IPv4 only in the supported subset, Q64).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct IPBlock {
    /// CIDR (e.g. `10.42.0.0/24`).
    #[serde(default)]
    pub cidr: String,
    /// Exceptions: CIDRs punched out of `cidr` (must be contained in it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub except: Option<Vec<String>>,
}

/// NetworkPolicyStatus: minimal optional conditions field for wire
/// compatibility (never written by init-pro; Q64).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct NetworkPolicyStatus {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conditions: Option<Vec<NetworkPolicyCondition>>,
}

/// One condition entry (upstream `metav1.Condition` shape, reduced).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct NetworkPolicyCondition {
    #[serde(default, rename = "type")]
    pub type_: String,
    #[serde(default)]
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_transition_time: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// LabelSelector is upstream `meta/v1.LabelSelector` re-declared in-tree
/// (Q32 doctrine: no k8s-openapi in the data plane). Matching semantics
/// live in [`super::network_policy_rules::label_selector_matches`] — note
/// that NetworkPolicy selectors use the upstream EMPTY-MATCHES-ALL rule.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct LabelSelector {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub match_labels: Option<std::collections::BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub match_expressions: Option<Vec<LabelSelectorRequirement>>,
}

/// LabelSelectorRequirement is one `key operator values` expression.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct LabelSelectorRequirement {
    #[serde(default)]
    pub key: String,
    #[serde(default)]
    pub operator: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub values: Option<Vec<String>>,
}
