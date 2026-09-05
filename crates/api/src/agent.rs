//! init-pro.io/v1 `Agent` first-class resource (T7.1 rewrite, Q35).
//! An Agent is a schedulable opencoder session: its spec carries the same
//! scheduling-alignment fields as a Pod (nodeName/nodeSelector/tolerations/
//! affinity/schedulerName/resources) plus the opencoder payload (agent preset,
//! prompt, model, provider). Scheduling runs through the same filter/score
//! loop as Pods; node-side execution is Phase B.

use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::{Affinity, ResourceRequirements, Toleration};
use kube_core::gvk::GroupVersionKind;
use kube_core::metadata::{ListMeta, ObjectMeta, TypeMeta};
use serde::{Deserialize, Serialize};

use crate::schema::{SchemaRegistry, Scope};

/// API group + version for init-pro's own resources (shared with
/// [`crate::initpro`]; the `init-pro.io/v1` group is the stable home for
/// first-class init-pro objects).
pub const GROUP: &str = "init-pro.io";
pub const VERSION: &str = "v1";
pub const API_VERSION: &str = "init-pro.io/v1";

/// Kind / plural for the `Agent` resource.
pub const KIND: &str = "Agent";
pub const PLURAL: &str = "agents";

/// Observed `status.phase` values for an `Agent` (Pod-lifecycle parity).
pub const PHASE_PENDING: &str = "Pending";
pub const PHASE_RUNNING: &str = "Running";
pub const PHASE_SUCCEEDED: &str = "Succeeded";
pub const PHASE_FAILED: &str = "Failed";

/// `Agent` — a schedulable opencoder session (T7.1 rewrite, Q35).
///
/// First-class Kubernetes object shape: flattened `TypeMeta`, `ObjectMeta`,
/// a typed spec, and an optional status — serde-compatible with
/// `kubectl apply` (JSON-only wire format, Q10).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Agent {
    #[serde(flatten)]
    pub types: TypeMeta,
    pub metadata: ObjectMeta,
    pub spec: AgentSpec,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub status: Option<AgentStatus>,
}

/// The desired Agent state: Pod scheduling-alignment fields plus the
/// opencoder payload. Cross-crate wire contract — the scheduler/controllers
/// depend on these exact JSON keys.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AgentSpec {
    /// Node the Agent must run on (Pod `spec.nodeName` parity). Empty = any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_name: Option<String>,
    /// Label selector the target node must satisfy (Pod `spec.nodeSelector`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub node_selector: BTreeMap<String, String>,
    /// Taints this Agent tolerates (Pod `spec.tolerations` parity).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tolerations: Vec<Toleration>,
    /// Node/pod affinity constraints (Pod `spec.affinity` parity).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub affinity: Option<Affinity>,
    /// Scheduler that owns placement (empty string = default scheduler).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub scheduler_name: String,
    /// Compute resources for the session (Pod container resources parity).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<ResourceRequirements>,
    /// opencoder agent preset to execute (e.g. `"opencoder"`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub agent: String,
    /// Prompt driving the session.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub prompt: String,
    /// Model override for the session (optional).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Provider override for the session (optional). Credentials never live
    /// here: API keys / base URLs travel via environment only (Q35).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
}

/// Observed Agent state.
///
/// Decoding `status: {}` (or any status without a `phase`) defaults the phase
/// to [`PHASE_PENDING`]; a missing `status` decodes to `None` so that a
/// status-less object round-trips losslessly.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct AgentStatus {
    /// Lifecycle phase (`Pending` until the scheduler binds a node).
    pub phase: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<AgentCondition>,
}

impl Default for AgentStatus {
    fn default() -> Self {
        Self {
            phase: PHASE_PENDING.to_string(),
            conditions: Vec::new(),
        }
    }
}

/// A minimal condition (subset of `meta/v1.Condition`) for status reporting;
/// identical shape to [`crate::initpro::Condition`].
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct AgentCondition {
    #[serde(rename = "type")]
    pub type_: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reason: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub message: String,
}

/// `AgentList` is a collection of Agent.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct AgentList {
    #[serde(flatten)]
    pub types: TypeMeta,
    #[serde(default)]
    pub metadata: ListMeta,
    #[serde(default)]
    pub items: Vec<Agent>,
}

/// Register the `Agent` type into a [`SchemaRegistry`] (short name `ag`).
pub fn register(registry: &mut SchemaRegistry) {
    registry.register_with_short_names(
        GROUP,
        VERSION,
        KIND,
        PLURAL,
        "AgentList",
        Scope::Namespaced,
        &["ag"],
    );
}

/// The GVK for `Agent` (convenience).
pub fn gvk() -> GroupVersionKind {
    GroupVersionKind::gvk(GROUP, VERSION, KIND)
}

/// Construct an `Agent` with correct `TypeMeta` and a `Pending` status.
pub fn new_agent(name: &str, namespace: &str, spec: AgentSpec) -> Agent {
    Agent {
        types: TypeMeta {
            api_version: API_VERSION.to_string(),
            kind: KIND.to_string(),
        },
        metadata: ObjectMeta {
            name: Some(name.to_string()),
            namespace: Some(namespace.to_string()),
            ..ObjectMeta::default()
        },
        spec,
        status: Some(AgentStatus::default()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::serde_ext::{from_json, round_trip, to_json, to_json_pretty};
    use k8s_openapi::api::core::v1::{
        NodeAffinity, NodeSelector, NodeSelectorRequirement, NodeSelectorTerm,
    };
    use k8s_openapi::apimachinery::pkg::api::resource::Quantity;

    /// An Agent with every spec/status field populated — the cross-crate
    /// wire-shape fixture for the scheduler/controllers contract.
    fn fully_populated() -> Agent {
        let mut agent = new_agent(
            "session-1",
            "default",
            AgentSpec {
                node_name: Some("node-1".to_string()),
                node_selector: BTreeMap::from([(
                    "kubernetes.io/os".to_string(),
                    "linux".to_string(),
                )]),
                tolerations: vec![Toleration {
                    effect: Some("NoSchedule".to_string()),
                    key: Some("gpu".to_string()),
                    operator: Some("Exists".to_string()),
                    toleration_seconds: None,
                    value: None,
                }],
                affinity: Some(Affinity {
                    node_affinity: Some(NodeAffinity {
                        preferred_during_scheduling_ignored_during_execution: None,
                        required_during_scheduling_ignored_during_execution: Some(NodeSelector {
                            node_selector_terms: vec![NodeSelectorTerm {
                                match_expressions: Some(vec![NodeSelectorRequirement {
                                    key: "kubernetes.io/os".to_string(),
                                    operator: "In".to_string(),
                                    values: Some(vec!["linux".to_string()]),
                                }]),
                                match_fields: None,
                            }],
                        }),
                    }),
                    pod_affinity: None,
                    pod_anti_affinity: None,
                }),
                scheduler_name: "init-pro-scheduler".to_string(),
                resources: Some(ResourceRequirements {
                    claims: None,
                    limits: Some(BTreeMap::from([
                        ("cpu".to_string(), Quantity("1".to_string())),
                        ("memory".to_string(), Quantity("512Mi".to_string())),
                    ])),
                    requests: Some(BTreeMap::from([
                        ("cpu".to_string(), Quantity("250m".to_string())),
                        ("memory".to_string(), Quantity("128Mi".to_string())),
                    ])),
                }),
                agent: "opencoder".to_string(),
                prompt: "fix the failing test".to_string(),
                model: Some("claude-sonnet-4-5".to_string()),
                provider: Some("anthropic".to_string()),
            },
        );
        agent.status = Some(AgentStatus {
            phase: PHASE_PENDING.to_string(),
            conditions: vec![AgentCondition {
                type_: "Scheduled".to_string(),
                status: "False".to_string(),
                reason: "Unschedulable".to_string(),
                message: "0/1 nodes available".to_string(),
            }],
        });
        agent
    }

    #[test]
    fn register_and_resolve_agent() {
        let mut reg = SchemaRegistry::new();
        register(&mut reg);
        let info = reg.get(&gvk()).expect("Agent registered");
        assert_eq!(info.kind, "Agent");
        assert_eq!(info.resource, "agents");
        assert_eq!(info.list_kind, "AgentList");
        assert_eq!(info.scope, Scope::Namespaced);
        assert!(info.short_names.iter().any(|s| s == "ag"));
    }

    #[test]
    fn agent_round_trip_is_lossless() {
        let agent = fully_populated();
        let back = round_trip(&agent).unwrap();
        assert_eq!(back, agent);
    }

    #[test]
    fn agent_wire_json_shape() {
        let agent = fully_populated();
        let s = String::from_utf8(to_json(&agent).unwrap()).unwrap();
        assert!(s.contains("\"apiVersion\":\"init-pro.io/v1\""));
        assert!(s.contains("\"kind\":\"Agent\""));
        assert!(s.contains("\"nodeName\":\"node-1\""));
        assert!(s.contains("\"nodeSelector\""));
        assert!(s.contains("\"schedulerName\":\"init-pro-scheduler\""));
        // A status-less Agent omits the field entirely (lossless wire shape).
        let mut no_status = agent.clone();
        no_status.status = None;
        let s = String::from_utf8(to_json(&no_status).unwrap()).unwrap();
        assert!(!s.contains("\"status\""));
        // Decoding an empty status defaults the phase to Pending.
        let empty: Agent = from_json(
            br#"{
            "apiVersion": "init-pro.io/v1",
            "kind": "Agent",
            "metadata": {"name": "s", "namespace": "default"},
            "spec": {"agent": "opencoder", "prompt": "hi"},
            "status": {}
        }"#,
        )
        .unwrap();
        assert_eq!(empty.status.unwrap().phase, PHASE_PENDING);
    }

    #[test]
    fn agent_decodes_from_kubectl_apply_json() {
        let json = r#"{
            "apiVersion": "init-pro.io/v1",
            "kind": "Agent",
            "metadata": {"name": "session-1", "namespace": "default"},
            "spec": {
                "agent": "opencoder",
                "prompt": "fix the failing test",
                "nodeName": "node-1"
            }
        }"#;
        let agent: Agent = from_json(json.as_bytes()).unwrap();
        assert_eq!(agent.metadata.name.as_deref(), Some("session-1"));
        assert_eq!(agent.spec.node_name.as_deref(), Some("node-1"));
        // Spec defaults kick in for omitted fields.
        assert!(agent.spec.node_selector.is_empty());
        assert!(agent.spec.tolerations.is_empty());
        assert!(agent.spec.scheduler_name.is_empty());
        // Status defaults to Pending via `AgentStatus::default()`.
        assert_eq!(agent.status.unwrap_or_default().phase, PHASE_PENDING);
    }

    #[test]
    fn agent_fully_populated_json_contract() {
        // Pins the exact wire JSON of a fully-populated Agent (the
        // cross-crate contract for the scheduler/controllers, Q35).
        let s = to_json_pretty(&fully_populated()).unwrap();
        println!("{s}");
        for fragment in [
            "\"apiVersion\": \"init-pro.io/v1\"",
            "\"kind\": \"Agent\"",
            "\"nodeName\": \"node-1\"",
            "\"schedulerName\": \"init-pro-scheduler\"",
            "\"phase\": \"Pending\"",
            "\"type\": \"Scheduled\"",
        ] {
            assert!(s.contains(fragment), "missing {fragment} in:\n{s}");
        }
    }
}
