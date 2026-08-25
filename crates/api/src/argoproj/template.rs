//! Argo Workflows `Template` + step/executor subtypes (TODO **T7.3 Scope A**,
//! decision **Q30**).
//!
//! 1:1 typed port of upstream `workflow_types.go` v4 (`Template`,
//! `WorkflowStep`, `Metadata`, `ScriptTemplate`, `ResourceTemplate`,
//! `ManifestFrom`, `UserContainer`, `ExecutorConfig`, `ExecutorPlugin*`),
//! the free-form `Plugin` template (`plugin_types.go`) and the executor
//! plugin spec (`pkg/plugins/spec/plugin_types.go`, mirrored as `mod exec`).
//! Field SSOT is the Go json tags; wire format is JSON-only (Q10). Data-only
//! port: Go helper methods (`GetType`, `SetType`, ...) are not carried.

use std::collections::BTreeMap;

use k8s_openapi::api::core::v1 as apiv1;
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use kube_core::metadata::ObjectMeta;
use serde::{Deserialize, Serialize};

use super::artifact::{Artifact, ArtifactLocation};
use super::container_set::ContainerSetTemplate;
use super::dag::{ContinueOn, DAGTemplate, SuspendTemplate, TemplateRef};
use super::http::HTTP;
use super::io::{Arguments, Data, Inputs, Outputs, Sequence};
use super::lifecycle::{LifecycleHooks, Memoize, RetryStrategy, Synchronization};
use super::metrics::Metrics;
use super::scalar::Item;

/// Template is a reusable and composable unit of execution in a workflow.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Template {
    /// Name is the name of the template.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    /// Inputs describe what inputs parameters and artifacts are supplied.
    #[serde(default)]
    pub inputs: Inputs,
    /// Outputs describe the parameters and artifacts this template produces.
    #[serde(default)]
    pub outputs: Outputs,
    /// NodeSelector schedules this step on the selected node(s).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub node_selector: BTreeMap<String, String>,
    /// Affinity sets the pod's scheduling constraints.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub affinity: Option<apiv1::Affinity>,
    /// Metadata sets the pod's metadata, i.e. annotations and labels.
    #[serde(default)]
    pub metadata: Metadata,
    /// Daemon lets the workflow proceed once the container reaches readiness.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon: Option<bool>,
    /// Steps define a series of sequential/parallel workflow steps.
    ///
    /// Go models this as `[]ParallelSteps` with a custom MarshalJSON that
    /// flattens each `ParallelSteps` to a bare `[step, ...]` array; the
    /// `Vec<Vec<WorkflowStep>>` shape here is wire-identical.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub steps: Vec<Vec<WorkflowStep>>,
    /// Container is the main container image to run in the pod.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container: Option<apiv1::Container>,
    /// ContainerSet groups multiple containers within a single pod.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container_set: Option<ContainerSetTemplate>,
    /// Script runs a portion of code against an interpreter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script: Option<ScriptTemplate>,
    /// Resource template subtype which can run k8s resources.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource: Option<ResourceTemplate>,
    /// DAG template subtype which runs a DAG.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dag: Option<DAGTemplate>,
    /// Suspend template subtype which can suspend a workflow at the step.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suspend: Option<SuspendTemplate>,
    /// Data is a data template.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Data>,
    /// HTTP makes an HTTP request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http: Option<HTTP>,
    /// Plugin is a plugin template (free-form structure, kept as raw JSON).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin: Option<Plugin>,
    /// Volumes is a list of volumes that can be mounted by containers.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub volumes: Vec<apiv1::Volume>,
    /// InitContainers run before the main container.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub init_containers: Vec<UserContainer>,
    /// Sidecars run alongside the main container and are killed when it completes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sidecars: Vec<UserContainer>,
    /// ArchiveLocation is where step files (logs, artifacts, ...) are stored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archive_location: Option<ArtifactLocation>,
    /// ActiveDeadlineSeconds terminates the pod after this duration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_deadline_seconds: Option<IntOrString>,
    /// RetryStrategy describes how to retry a template when it fails.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_strategy: Option<RetryStrategy>,
    /// Parallelism limits max total parallel pods executing at once.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallelism: Option<i64>,
    /// FailFast fails this template if any child pod has failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fail_fast: Option<bool>,
    /// Tolerations to apply to workflow pods.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tolerations: Vec<apiv1::Toleration>,
    /// SchedulerName dispatches the pod via the specified scheduler.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub scheduler_name: String,
    /// PriorityClassName is the pod's priority class.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub priority_class_name: String,
    /// ServiceAccountName to run the pod as.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub service_account_name: String,
    /// AutomountServiceAccountToken mounts a service account token in pods.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub automount_service_account_token: Option<bool>,
    /// Executor holds configurations of the executor container.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executor: Option<ExecutorConfig>,
    /// HostAliases to modify the pod's /etc/hosts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub host_aliases: Vec<apiv1::HostAlias>,
    /// SecurityContext holds pod-level security attributes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub security_context: Option<apiv1::PodSecurityContext>,
    /// PodSpecPatch holds a strategic merge patch to apply to the pod spec.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub pod_spec_patch: String,
    /// Metrics are a list of metrics emitted from this template.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metrics: Option<Metrics>,
    /// Synchronization holds lock configuration for this template.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub synchronization: Option<Synchronization>,
    /// Memoize allows reuse of outputs from already executed templates.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memoize: Option<Memoize>,
    /// Timeout is the total node execution timeout from the node's start.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub timeout: String,
    /// Annotations to add to the template at runtime.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub annotations: BTreeMap<String, String>,
    /// PendingTimeout caps time spent in Pending status.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub pending_timeout: String,
    /// PodResources defines pod-level resource requests and limits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pod_resources: Option<apiv1::ResourceRequirements>,
    /// ResourceClaims that must be allocated before the pod starts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub resource_claims: Vec<apiv1::PodResourceClaim>,
}

/// Metadata is pod metadata.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Metadata {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub annotations: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
}

/// WorkflowStep is a reference to a template to execute in a series of steps.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowStep {
    /// Name of the step.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    /// Template is the name of the template to execute as the step.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub template: String,
    /// Inline is the template itself (recursive — hence boxed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inline: Option<Box<Template>>,
    /// Arguments hold the parameters and artifacts passed to the template.
    #[serde(default)]
    pub arguments: Arguments,
    /// TemplateRef references a template in another resource.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template_ref: Option<TemplateRef>,
    /// WithItems expands the step into parallel steps, one per item.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub with_items: Vec<Item>,
    /// WithParam expands the step from a JSON list parameter.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub with_param: String,
    /// WithSequence expands the step from a number sequence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub with_sequence: Option<Sequence>,
    /// When is a conditional expression guarding step execution.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub when: String,
    /// ContinueOn keeps the workflow running if the step fails/errors.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continue_on: Option<ContinueOn>,
    /// OnExit is a template name invoked when the step exits.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub on_exit: String,
    /// Hooks binds lifecycle events to templates.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub hooks: LifecycleHooks,
}

/// ScriptTemplate is a template subtype to enable scripting through code steps.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ScriptTemplate {
    /// Inlined upstream `corev1.Container` (Go: `Container \`json:",inline"\``).
    #[serde(flatten)]
    pub container: apiv1::Container,
    /// Source contains the source code of the script to execute.
    #[serde(default)]
    pub source: String,
}

/// ResourceTemplate is a template subtype to manipulate kubernetes resources.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ResourceTemplate {
    /// Action is the action to perform: get, create, apply, delete, replace, patch.
    #[serde(default)]
    pub action: String,
    /// MergeStrategy specifies how to merge the manifest with the live object.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub merge_strategy: String,
    /// Manifest contains the resource to create (mutually exclusive with `manifestFrom`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub manifest: String,
    /// ManifestFrom sources the manifest from an artifact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manifest_from: Option<ManifestFrom>,
    /// SetOwnerReference sets the workflow as the resource's owner.
    #[serde(default, skip_serializing_if = "super::is_false")]
    pub set_owner_reference: bool,
    /// SuccessCondition is an expression that marks the resource successful.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub success_condition: String,
    /// FailureCondition is an expression that marks the resource failed.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub failure_condition: String,
    /// Flags are CLI flags to pass to `kubectl`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub flags: Vec<String>,
}

/// ManifestFrom references an artifact a resource manifest is sourced from.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ManifestFrom {
    #[serde(default)]
    pub artifact: Option<Artifact>,
}

/// UserContainer is a container whose life-cycle is managed by the workflow.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct UserContainer {
    /// Inlined upstream `corev1.Container` (Go: `Container \`json:",inline"\``).
    #[serde(flatten)]
    pub container: apiv1::Container,
    /// MirrorVolumeMounts mounts the main container's volumes at same paths.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mirror_volume_mounts: Option<bool>,
}

/// ExecutorConfig holds configurations of an executor container.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ExecutorConfig {
    /// ServiceAccountName of the executor container.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub service_account_name: String,
}

/// Plugin is a plugin template: an Object with exactly one key (free-form).
/// Upstream stores the raw JSON object and validates the exactly-one-key
/// shape in `UnmarshalJSON`; this data-only port passes the raw JSON through
/// unchanged (tolerant, constraint not enforced here).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(transparent)]
pub struct Plugin(pub serde_json::Value);

/// ExecutorPlugin describes a workflow-level executor plugin.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ExecutorPlugin {
    #[serde(default)]
    pub metadata: ObjectMeta,
    #[serde(default)]
    pub spec: ExecutorPluginSpec,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ExecutorPluginSpec {
    #[serde(default)]
    pub sidecar: ExecutorPluginSidecar,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ExecutorPluginSidecar {
    /// AutomountServiceAccountToken enables mounting the service account
    /// token (service account must be `<plugin-name>-executor-plugin`).
    #[serde(default, skip_serializing_if = "super::is_false")]
    pub automount_service_account_token: bool,
    /// Container is the Kubernetes container spec for the sidecar.
    #[serde(default)]
    pub container: apiv1::Container,
}

/// Mirror of upstream `pkg/plugins/spec/plugin_types.go` — the executor
/// plugin contract the workflow executor speaks (TODO **T7.3 Scope A**, Q30).
pub mod exec {
    use k8s_openapi::api::core::v1 as apiv1;
    use kube_core::metadata::{ObjectMeta, TypeMeta};
    use serde::{Deserialize, Serialize};

    use crate::argoproj::is_false;

    /// Plugin is the executor plugin object (Go `spec.Plugin`).
    #[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
    #[serde(rename_all = "camelCase")]
    pub struct Plugin {
        #[serde(flatten)]
        pub types: TypeMeta,
        #[serde(default)]
        pub metadata: ObjectMeta,
        #[serde(default)]
        pub spec: PluginSpec,
    }

    /// PluginSpec declares the plugin's sidecar (Go `spec.PluginSpec`).
    #[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
    #[serde(rename_all = "camelCase")]
    pub struct PluginSpec {
        #[serde(default)]
        pub sidecar: Sidecar,
    }

    /// Sidecar serves executor plugin requests (Go `spec.Sidecar`).
    #[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
    #[serde(rename_all = "camelCase")]
    pub struct Sidecar {
        /// AutomountServiceAccountToken mounts the service account token
        /// (same name as the plugin).
        #[serde(default, skip_serializing_if = "is_false")]
        pub automount_service_account_token: bool,
        /// Container is the Kubernetes container spec for the sidecar.
        #[serde(default)]
        pub container: apiv1::Container,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_steps_round_trip_as_bare_arrays() {
        let step = WorkflowStep {
            name: "step-1".to_string(),
            template: "echo".to_string(),
            ..WorkflowStep::default()
        };
        let tmpl = Template {
            name: "steps-tmpl".to_string(),
            steps: vec![vec![step]],
            ..Template::default()
        };
        let json = serde_json::to_value(&tmpl).unwrap();
        assert_eq!(json["steps"][0][0]["name"].as_str(), Some("step-1"));
        assert_eq!(json["steps"][0][0]["template"].as_str(), Some("echo"));
        let back: Template = serde_json::from_value(json).unwrap();
        assert_eq!(back, tmpl);
    }
}
