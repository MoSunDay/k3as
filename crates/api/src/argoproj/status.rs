//! Argo Workflows status types (TODO **T7.3 Scope A**, decision **Q30**).
//!
//! 1:1 typed port of the workflow/node status model in upstream
//! `workflow_types.go` v4 (`WorkflowStatus`, `NodeStatus`, `Condition`,
//! `ArtGCStatus`, artifact-search types). Field-level SSOT is the Go json
//! tags; wire format is JSON-only (Q10). Data-only port: the Go
//! `Nodes`/`Conditions` helper methods are not carried.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use k8s_openapi::api::core::v1 as apiv1;
use k8s_openapi::apimachinery::pkg::apis::meta::v1 as metav1;

use super::artifact::{Artifact, ArtifactRepositoryRefStatus};
use super::dag::TemplateRef;
use super::io::{Inputs, Outputs};
use super::is_false;
use super::is_zero_i32;
use super::is_zero_i64;
use super::lifecycle::MemoizationStatus;
use super::phases::{ArtifactGCStrategy, ConditionType, NodePhase, NodeType, WorkflowPhase};
use super::scalar::{EstimatedDuration, Progress, ResourcesDuration};
use super::sync::{NodeFlag, NodeSynchronizationStatus, SynchronizationStatus};
use super::template::Template;
use super::workflow::WorkflowSpec;

/// Nodes is a mapping between a node ID and the node's status.
pub type Nodes = BTreeMap<String, NodeStatus>;

/// Conditions is a list of conditions the Workflow may have.
pub type Conditions = Vec<Condition>;

/// ArtGCStatus maintains state related to ArtifactGC.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ArtGCStatus {
    /// Have Pods been started to perform this strategy? (avoids re-processing)
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub strategies_processed: BTreeMap<ArtifactGCStrategy, bool>,
    /// Have completed Pods been processed? (mapped by Pod name)
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub pods_recouped: BTreeMap<String, bool>,
    /// True when we already checked whether GC is needed and it is not.
    #[serde(default, skip_serializing_if = "is_false")]
    pub not_specified: bool,
}

/// ArtifactSearchQuery identifies artifacts for GC processing.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactSearchQuery {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    #[serde(rename = "artifactGCStrategies")]
    pub artifact_gc_strategies: BTreeMap<ArtifactGCStrategy, bool>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub artifact_name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub template_name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub node_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deleted: Option<bool>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub node_types: BTreeMap<NodeType, bool>,
}

/// ArtifactSearchResult is an `Artifact` matched by an [`ArtifactSearchQuery`].
/// Go embeds `Artifact` with no json tag, so its fields inline on the wire;
/// `NodeID` likewise carries no json tag and marshals under its exact name.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct ArtifactSearchResult {
    #[serde(flatten)]
    pub artifact: Artifact,
    #[serde(rename = "NodeID")]
    pub node_id: String,
}

/// ArtifactSearchResults is a list of artifact search results.
pub type ArtifactSearchResults = Vec<ArtifactSearchResult>;

/// Condition contains details about the state of a workflow condition.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Condition {
    /// Type is the type of condition (see `super::phases` constants).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub r#type: ConditionType,
    /// Status of the condition — "True", "False" or "Unknown"
    /// (Go `metav1.ConditionStatus`; `k8s_openapi` models it as a String).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub status: String,
    /// Message is the condition message.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub message: String,
}

/// WorkflowStatus is the status of a workflow.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowStatus {
    /// High-level lifecycle summary: ""/"Pending"/"Running" before
    /// completion, "Succeeded"/"Failed"/"Error" after.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub phase: WorkflowPhase,
    /// Time at which this workflow started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<metav1::Time>,
    /// Time at which this workflow completed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<metav1::Time>,
    /// Estimated duration in seconds.
    #[serde(default, skip_serializing_if = "is_zero_i64")]
    pub estimated_duration: EstimatedDuration,
    /// Progress to completion (e.g. "1/2").
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub progress: Progress,
    /// Human readable message about why the workflow is in this condition.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub message: String,
    /// Compressed and base64 decoded Nodes map.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub compressed_nodes: String,
    /// Mapping between a node ID and the node's status.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub nodes: Nodes,
    /// Set when node status has been offloaded to a database (holds a hash
    /// of the offloaded data); then `nodes`/`compressedNodes` are empty.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub offload_node_status_version: String,
    /// Mapping between a template ref and the stored template.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub stored_templates: BTreeMap<String, Template>,
    /// PVCs created as part of the workflow; drained at workflow end.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub persistent_volume_claims: Vec<apiv1::Volume>,
    /// Output values and artifact locations produced via global outputs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outputs: Option<Outputs>,
    /// Conditions is a list of conditions the Workflow may have.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Conditions,
    /// ResourcesDuration is the total for the workflow.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub resources_duration: ResourcesDuration,
    /// Stores the WorkflowTemplate spec for future execution (Go field
    /// `StoredWorkflowSpec`, json tag `storedWorkflowTemplateSpec`).
    #[serde(
        default,
        rename = "storedWorkflowTemplateSpec",
        skip_serializing_if = "Option::is_none"
    )]
    pub stored_workflow_spec: Option<WorkflowSpec>,
    /// Status of synchronization locks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub synchronization: Option<SynchronizationStatus>,
    /// Cached repository in use, so it need not be re-resolved each
    /// reconcile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_repository_ref: Option<ArtifactRepositoryRefStatus>,
    /// Status of Artifact Garbage Collection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(rename = "artifactGCStatus")]
    pub artifact_gc_status: Option<ArtGCStatus>,
    /// Task result completion status (mapped by node ID); prevents
    /// premature archiving and garbage collection.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub task_results_completion_status: BTreeMap<String, bool>,
}

/// NodeStatus contains status information about an individual node in the
/// workflow.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct NodeStatus {
    /// ID is a unique identifier of a node within the workflow
    /// (deterministic hash of the node name).
    #[serde(default)]
    pub id: String,
    /// Name is unique name in the node tree used to generate the node ID.
    #[serde(default)]
    pub name: String,
    /// Human readable representation, unique within a template boundary.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub display_name: String,
    /// Type indicates type of node.
    #[serde(default)]
    pub r#type: NodeType,
    /// Template name this node corresponds to (not for virtual nodes).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub template_name: String,
    /// Reference to the template resource this node corresponds to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template_ref: Option<TemplateRef>,
    /// Template scope in which this node's template was retrieved.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub template_scope: String,
    /// Lifecycle state machine state: "Pending"/"Running" before
    /// completion; "Succeeded"/"Skipped"/"Failed"/"Error"/"Omitted" final.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub phase: NodePhase,
    /// Node ID of the associated template root node this node belongs to.
    #[serde(
        default,
        rename = "boundaryID",
        skip_serializing_if = "String::is_empty"
    )]
    pub boundary_id: String,
    /// Human readable message about why the node is in this condition.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub message: String,
    /// Time at which this node started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<metav1::Time>,
    /// Time at which this node completed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<metav1::Time>,
    /// Estimated duration in seconds.
    #[serde(default, skip_serializing_if = "is_zero_i64")]
    pub estimated_duration: EstimatedDuration,
    /// Progress to completion.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub progress: Progress,
    /// Indicative (not accurate) resource duration; populated on completion.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub resources_duration: ResourcesDuration,
    /// PodIP captures the IP of the pod for daemoned steps.
    #[serde(default, rename = "podIP", skip_serializing_if = "String::is_empty")]
    pub pod_ip: String,
    /// Tracks whether this node was daemoned and needs to be terminated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemoned: Option<bool>,
    /// Tracks some history of the node (hooked, retried, etc.).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_flag: Option<NodeFlag>,
    /// Input parameter values and artifact locations supplied to this
    /// template invocation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inputs: Option<Inputs>,
    /// Output parameter values and artifact locations produced by this
    /// template invocation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outputs: Option<Outputs>,
    /// Children is a list of child node IDs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<String>,
    /// Node IDs considered "outbound" of this template invocation (the
    /// last nodes to run before the template is complete).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub outbound_nodes: Vec<String>,
    /// Name of the Kubernetes node the Pod is running on, if applicable.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub host_node_name: String,
    /// Holds information about cached nodes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memoization_status: Option<MemoizationStatus>,
    /// Synchronization status of the node.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub synchronization_status: Option<NodeSynchronizationStatus>,
    /// Whether the node's output has been received.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_result_synced: Option<bool>,
    /// Number of pod restarts due to infrastructure failures before the
    /// main container started.
    #[serde(default, skip_serializing_if = "is_zero_i32")]
    pub failed_pod_restarts: i32,
    /// UID of the pod currently being restarted; prevents duplicate restart
    /// attempts and is cleared when the replacement pod starts running.
    #[serde(
        default,
        rename = "restartingPodUID",
        skip_serializing_if = "String::is_empty"
    )]
    pub restarting_pod_uid: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal status with phase, a start time and one node decodes and
    /// re-encodes losslessly (Q10 wire contract).
    #[test]
    fn workflow_status_round_trip() {
        let value = serde_json::json!({
            "phase": "Running",
            "startedAt": "2026-08-25T10:00:00Z",
            "nodes": {
                "wf-1234567890": {
                    "id": "wf-1234567890",
                    "name": "wf-1234567890",
                    "type": "Pod",
                    "phase": "Running"
                }
            },
            "taskResultsCompletionStatus": { "wf-1234567890": true }
        });
        let status: WorkflowStatus =
            serde_json::from_value(value.clone()).expect("decode WorkflowStatus");
        assert_eq!(status.phase, "Running");
        assert_eq!(status.nodes.len(), 1);
        assert_eq!(
            status.task_results_completion_status.get("wf-1234567890"),
            Some(&true)
        );
        assert_eq!(serde_json::to_value(&status).expect("encode"), value);
    }

    /// Non-camelCase Go json tags keep their exact wire keys, and the
    /// untagged `ArtifactSearchResult` fields marshal as Go does.
    #[test]
    fn exact_wire_keys() {
        let node = NodeStatus {
            boundary_id: "b1".into(),
            pod_ip: "10.0.0.1".into(),
            restarting_pod_uid: "uid-1".into(),
            ..Default::default()
        };
        let v = serde_json::to_value(&node).expect("encode node");
        assert_eq!(v["boundaryID"], "b1");
        assert_eq!(v["podIP"], "10.0.0.1");
        assert_eq!(v["restartingPodUID"], "uid-1");
        // no-omitempty fields always present; omitempty empties dropped
        assert!(v.get("id").is_some());
        assert!(v.get("message").is_none());
        let back: NodeStatus = serde_json::from_value(v).expect("decode node");
        assert_eq!(back, node);
        // Embedded Artifact inlines (flatten); NodeID marshals as-is.
        let result = ArtifactSearchResult {
            artifact: Artifact {
                name: "out".into(),
                ..Default::default()
            },
            node_id: "n1".into(),
        };
        let v = serde_json::to_value(&result).expect("encode result");
        assert_eq!(v["name"], "out");
        assert_eq!(v["NodeID"], "n1");
        assert_eq!(
            serde_json::from_value::<ArtifactSearchResult>(v).expect("decode result"),
            result
        );
    }
}
