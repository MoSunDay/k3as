//! Argo Workflows `Workflow` + `WorkflowSpec` (TODO **T7.3 Scope A**, Q30).
//!
//! 1:1 typed port of upstream `workflow_types.go` v4 (`Workflow`,
//! `WorkflowList`, `Workflows`, `WorkflowSpec`, `WorkflowMetadata`,
//! `LabelValueFrom`, `TTLStrategy`). Field-level SSOT is the Go json tags;
//! wire format is JSON-only (Q10). Data-only port: Go helper methods
//! (`GetTTLStrategy`, `Workflows` sorting, ...) are not carried.

use std::collections::BTreeMap;

use k8s_openapi::api::core::v1 as apiv1;
use k8s_openapi::api::policy::v1 as policyv1;
use kube_core::metadata::{ListMeta, ObjectMeta, TypeMeta};
use serde::{Deserialize, Serialize};

use super::artifact::ArtifactRepositoryRef;
use super::dag::WorkflowTemplateRef;
use super::io::Arguments;
use super::lifecycle::{
    LifecycleHooks, PodGC, RetryStrategy, Synchronization, VolumeClaimGC, WorkflowLevelArtifactGC,
};
use super::metrics::Metrics;
use super::phases::ShutdownStrategy;
use super::status::WorkflowStatus;
use super::template::{ExecutorConfig, ExecutorPlugin, Metadata, Template};

/// Workflow is the top-level object: a directed acyclic graph of tasks.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Workflow {
    #[serde(flatten)]
    pub types: TypeMeta,
    #[serde(default)]
    pub metadata: ObjectMeta,
    /// Spec is the desired workflow behaviour (always serialized; Go
    /// `json:"spec"` has no omitempty).
    #[serde(default)]
    pub spec: WorkflowSpec,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<WorkflowStatus>,
}

/// Workflows is a list of Workflow resources (Go sort interface dropped).
pub type Workflows = Vec<Workflow>;

/// WorkflowList is list of Workflow resources.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowList {
    #[serde(flatten)]
    pub types: TypeMeta,
    #[serde(default)]
    pub metadata: ListMeta,
    #[serde(default)]
    pub items: Workflows,
}

/// TTLStrategy is the time to live after completion, success or failure.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TTLStrategy {
    /// SecondsAfterCompletion is the number of seconds to live after
    /// completion.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seconds_after_completion: Option<i32>,
    /// SecondsAfterSuccess is the number of seconds to live after success.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seconds_after_success: Option<i32>,
    /// SecondsAfterFailure is the number of seconds to live after failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seconds_after_failure: Option<i32>,
}

/// WorkflowSpec is the specification of a Workflow.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowSpec {
    /// Templates is a list of workflow templates used in a workflow.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub templates: Vec<Template>,
    /// Entrypoint is a template reference to the starting point of the
    /// workflow.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub entrypoint: String,
    /// Arguments contain the parameters and artifacts sent to the workflow
    /// entrypoint, referencable globally as `workflow.parameters.<name>`.
    #[serde(default)]
    pub arguments: Arguments,
    /// ServiceAccountName is the name of the ServiceAccount to run all pods
    /// of the workflow as.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub service_account_name: String,
    /// AutomountServiceAccountToken indicates whether a service account
    /// token should be automatically mounted in pods. ExecutorConfig
    /// ServiceAccountName must be specified if this value is false.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub automount_service_account_token: Option<bool>,
    /// Executor holds configurations for the executor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executor: Option<ExecutorConfig>,
    /// Volumes is a list of volumes that can be mounted by containers in a
    /// workflow.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub volumes: Vec<apiv1::Volume>,
    /// VolumeClaimTemplates is a list of claims that containers are allowed
    /// to reference. Every claim will be created for each Pod created by the
    /// corresponding Node.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub volume_claim_templates: Vec<apiv1::PersistentVolumeClaim>,
    /// Parallelism limits the max total parallel templates that can execute
    /// at the same time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallelism: Option<i64>,
    /// ArtifactRepositoryRef specifies the artifact repository to use for
    /// this workflow.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_repository_ref: Option<ArtifactRepositoryRef>,
    /// Suspend will suspend the workflow and prevent execution of any future
    /// templates in a workflow.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suspend: Option<bool>,
    /// NodeSelector is a selector which will result in all pods of the
    /// workflow to be scheduled on the selected node(s).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub node_selector: BTreeMap<String, String>,
    /// Affinity sets the scheduling constraints for all pods in the workflow.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub affinity: Option<apiv1::Affinity>,
    /// Tolerations to apply to workflow pods.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tolerations: Vec<apiv1::Toleration>,
    /// ImagePullSecrets is a list of references to secrets for pulling
    /// images used by this workflow.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub image_pull_secrets: Vec<apiv1::LocalObjectReference>,
    /// HostNetwork is a flag for enabling the host network mode (pods run
    /// with the host's network namespaces).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_network: Option<bool>,
    /// DNSPolicy (ClusterFirst or Default) to apply to workflow pods.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dns_policy: Option<String>,
    /// DNSConfig to apply to workflow pods.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dns_config: Option<apiv1::PodDNSConfig>,
    /// OnExit is a template reference invoked at the end of the workflow.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub on_exit: String,
    /// TTLStrategy limits the lifetime of a Workflow that has finished
    /// execution depending on success/failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_strategy: Option<TTLStrategy>,
    /// ActiveDeadlineSeconds is the duration the workflow may be active
    /// before it is terminated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_deadline_seconds: Option<i64>,
    /// Priority is used if two workflows are queued at the same time; the
    /// higher priority one runs first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<i32>,
    /// SchedulerName to schedule workflow pods.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub scheduler_name: String,
    /// PodGC describes the strategy for deleting completed pods.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(rename = "podGC")]
    pub pod_gc: Option<PodGC>,
    /// PodPriorityClassName applied to workflow pods.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub pod_priority_class_name: String,
    /// HostAliases to add to workflow pods' /etc/hosts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub host_aliases: Vec<apiv1::HostAlias>,
    /// SecurityContext holds pod-level security attributes for workflow
    /// pods.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub security_context: Option<apiv1::PodSecurityContext>,
    /// PodSpecPatch holds strategic merge patch to apply against the pod
    /// spec.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub pod_spec_patch: String,
    /// PodDisruptionBudget holds PDB config for workflow pods (MinAvailable
    /// / MaxUnavailable + a LabelSelector).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pod_disruption_budget: Option<policyv1::PodDisruptionBudgetSpec>,
    /// Metrics are metrics emitted from this Workflow.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metrics: Option<Metrics>,
    /// Shutdown will shutdown the workflow according to its
    /// ShutdownStrategy (`Terminate` or `Stop`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub shutdown: ShutdownStrategy,
    /// WorkflowTemplateRef points to a WorkflowTemplate resource; the
    /// workflow inherits its spec fields.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow_template_ref: Option<WorkflowTemplateRef>,
    /// Synchronization holds synchronization configuration (mutex/semaphore)
    /// for this workflow.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub synchronization: Option<Synchronization>,
    /// VolumeClaimGC describes the strategy for deleting PVCs from
    /// VolumeClaimTemplates.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(rename = "volumeClaimGC")]
    pub volume_claim_gc: Option<VolumeClaimGC>,
    /// RetryStrategy for all templates in the workflow (overridden per
    /// template).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_strategy: Option<RetryStrategy>,
    /// PodMetadata defines labels/annotations to add to workflow pods.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pod_metadata: Option<Metadata>,
    /// TemplateDefaults holds default field values applied to all templates.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template_defaults: Option<Template>,
    /// ArchiveLogs indicates the container logs should be archived.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archive_logs: Option<bool>,
    /// Hooks holds the lifecycle hooks being invoked on the workflow.
    #[serde(default, skip_serializing_if = "LifecycleHooks::is_empty")]
    pub hooks: LifecycleHooks,
    /// WorkflowMetadata contains some metadata of the workflow to refer to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow_metadata: Option<WorkflowMetadata>,
    /// ArtifactGC describes the strategy for deleting artifacts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(rename = "artifactGC")]
    pub artifact_gc: Option<WorkflowLevelArtifactGC>,
    /// ExecutorPlugins allows specifying an executor plugin at the workflow
    /// level (ARGO_WORKFLOW_LEVEL_EXECUTOR_PLUGINS feature gate); overrides
    /// controller ConfigMap executor plugin settings.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub executor_plugins: Vec<ExecutorPlugin>,
    /// PodResources defines pod-level resource requests/limits for all
    /// workflow pods (overridden by a template's podResources; requires the
    /// PodLevelResources feature gate).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pod_resources: Option<apiv1::ResourceRequirements>,
    /// ResourceClaims that must be allocated before this workflow's pods
    /// start; replaced as a whole rather than merged. Requires the
    /// DynamicResourceAllocation feature gate.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub resource_claims: Vec<apiv1::PodResourceClaim>,
}

/// WorkflowMetadata contains some metadata of the workflow to refer to.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowMetadata {
    /// Labels to apply to the workflow.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
    /// Annotations to apply to the workflow.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub annotations: BTreeMap<String, String>,
    /// LabelsFrom collects labels produced from expressions.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels_from: BTreeMap<String, LabelValueFrom>,
}

/// LabelValueFrom describes how a label value is computed from an expression
/// (expr-lang).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct LabelValueFrom {
    /// Expression to evaluate for the label value.
    #[serde(default)]
    pub expression: String,
}
