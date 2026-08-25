//! Argo Workflows string-enum phases and strategies (TODO **T7.3 Scope A**, decision **Q30**).
//!
//! 1:1 port of the upstream Go named string types and their const blocks
//! (`workflow_types.go`, `cron_workflow_types.go`, `workflow_phase.go`).
//! Per the port spec, Go named types become transparent Rust type aliases
//! (`pub type X = String;`) so the JSON wire format is unchanged (Q10); each
//! Go const block becomes `pub const <TYPE>_<NAME>: &str = ...;` constants
//! mirroring upstream values one-for-one — including which consts exist and
//! the empty-string "unset" sentinels.

/// TemplateType is the type of a template.
pub type TemplateType = String;

/// Possible template types.
pub const TEMPLATE_TYPE_CONTAINER: &str = "Container";
/// A container template.
pub const TEMPLATE_TYPE_CONTAINER_SET: &str = "ContainerSet";
/// A steps template.
pub const TEMPLATE_TYPE_STEPS: &str = "Steps";
/// A script template.
pub const TEMPLATE_TYPE_SCRIPT: &str = "Script";
/// A resource template.
pub const TEMPLATE_TYPE_RESOURCE: &str = "Resource";
/// A DAG template.
pub const TEMPLATE_TYPE_DAG: &str = "DAG";
/// A suspend template.
pub const TEMPLATE_TYPE_SUSPEND: &str = "Suspend";
/// A data template.
pub const TEMPLATE_TYPE_DATA: &str = "Data";
/// An HTTP template.
pub const TEMPLATE_TYPE_HTTP: &str = "HTTP";
/// A plugin template.
pub const TEMPLATE_TYPE_PLUGIN: &str = "Plugin";
/// Unknown template type.
pub const TEMPLATE_TYPE_UNKNOWN: &str = "Unknown";

/// NodePhase is a label for the condition of a node at the current time.
pub type NodePhase = String;

/// Node is waiting to run.
pub const NODE_PENDING: &str = "Pending";
/// Node is running.
pub const NODE_RUNNING: &str = "Running";
/// Node finished with no errors.
pub const NODE_SUCCEEDED: &str = "Succeeded";
/// Node was skipped.
pub const NODE_SKIPPED: &str = "Skipped";
/// Node or child of node exited with non-0 code.
pub const NODE_FAILED: &str = "Failed";
/// Node had an error other than a non 0 exit code.
pub const NODE_ERROR: &str = "Error";
/// Node was omitted because its `depends` condition was not met.
pub const NODE_OMITTED: &str = "Omitted";

/// NodeType is the type of a node.
pub type NodeType = String;

/// Node types.
pub const NODE_TYPE_POD: &str = "Pod";
/// A container node.
pub const NODE_TYPE_CONTAINER: &str = "Container";
/// A steps node.
pub const NODE_TYPE_STEPS: &str = "Steps";
/// A step group node.
pub const NODE_TYPE_STEP_GROUP: &str = "StepGroup";
/// A DAG node.
pub const NODE_TYPE_DAG: &str = "DAG";
/// A task group node.
pub const NODE_TYPE_TASK_GROUP: &str = "TaskGroup";
/// A retry node.
pub const NODE_TYPE_RETRY: &str = "Retry";
/// A skipped node.
pub const NODE_TYPE_SKIPPED: &str = "Skipped";
/// A suspend node.
pub const NODE_TYPE_SUSPEND: &str = "Suspend";
/// An HTTP node.
pub const NODE_TYPE_HTTP: &str = "HTTP";
/// A plugin node.
pub const NODE_TYPE_PLUGIN: &str = "Plugin";

/// ArtifactGCStrategy is the strategy when to delete artifacts for GC.
pub type ArtifactGCStrategy = String;

/// Delete artifacts on workflow completion.
pub const ARTIFACT_GC_ON_WORKFLOW_COMPLETION: &str = "OnWorkflowCompletion";
/// Delete artifacts on workflow deletion.
pub const ARTIFACT_GC_ON_WORKFLOW_DELETION: &str = "OnWorkflowDeletion";
/// Never delete artifacts.
pub const ARTIFACT_GC_NEVER: &str = "Never";
/// Artifact GC strategy unset.
pub const ARTIFACT_GC_STRATEGY_UNDEFINED: &str = "";

/// PodGCStrategy is the strategy to use when deleting completed pods.
pub type PodGCStrategy = String;

/// PodGCStrategy.
pub const POD_GC_ON_POD_NONE: &str = "";
/// Delete pods on pod completion.
pub const POD_GC_ON_POD_COMPLETION: &str = "OnPodCompletion";
/// Delete pods on pod success.
pub const POD_GC_ON_POD_SUCCESS: &str = "OnPodSuccess";
/// Delete pods on workflow completion.
pub const POD_GC_ON_WORKFLOW_COMPLETION: &str = "OnWorkflowCompletion";
/// Delete pods on workflow success.
pub const POD_GC_ON_WORKFLOW_SUCCESS: &str = "OnWorkflowSuccess";

/// VolumeClaimGCStrategy is the strategy to use when deleting volumes from completed workflows.
pub type VolumeClaimGCStrategy = String;

/// Delete volume claims on workflow completion.
pub const VOLUME_CLAIM_GC_ON_COMPLETION: &str = "OnWorkflowCompletion";
/// Delete volume claims on workflow success.
pub const VOLUME_CLAIM_GC_ON_SUCCESS: &str = "OnWorkflowSuccess";

/// ShutdownStrategy is how a workflow should be shutdown.
pub type ShutdownStrategy = String;

/// Terminate the workflow.
pub const SHUTDOWN_STRATEGY_TERMINATE: &str = "Terminate";
/// Stop the workflow.
pub const SHUTDOWN_STRATEGY_STOP: &str = "Stop";
/// No shutdown strategy.
pub const SHUTDOWN_STRATEGY_NONE: &str = "";

/// RetryPolicy is a strategy to use when retrying a template.
pub type RetryPolicy = String;

/// Always retry.
pub const RETRY_POLICY_ALWAYS: &str = "Always";
/// Retry on failure.
pub const RETRY_POLICY_ON_FAILURE: &str = "OnFailure";
/// Retry on error.
pub const RETRY_POLICY_ON_ERROR: &str = "OnError";
/// Retry on transient error.
pub const RETRY_POLICY_ON_TRANSIENT_ERROR: &str = "OnTransientError";

/// MetricType is the type of a metric (Gauge, Histogram, or Counter).
pub type MetricType = String;

/// Metric types.
pub const METRIC_TYPE_GAUGE: &str = "Gauge";
/// A histogram metric.
pub const METRIC_TYPE_HISTOGRAM: &str = "Histogram";
/// A counter metric.
pub const METRIC_TYPE_COUNTER: &str = "Counter";
/// An unknown metric type.
pub const METRIC_TYPE_UNKNOWN: &str = "Unknown";

/// GaugeOperation is the operation to apply on a Gauge metric.
pub type GaugeOperation = String;

/// Gauge operations.
pub const GAUGE_OPERATION_SET: &str = "Set";
/// Add to the gauge value.
pub const GAUGE_OPERATION_ADD: &str = "Add";
/// Subtract from the gauge value.
pub const GAUGE_OPERATION_SUB: &str = "Sub";

/// ArtifactPluginName is the name of an artifact plugin (upstream defines the
/// named string type with NO const block — free-form plugin names).
pub type ArtifactPluginName = String;

/// LifecycleEvent is a lifestyle event to hook into.
pub type LifecycleEvent = String;

/// Exit lifecycle event.
pub const LIFECYCLE_EVENT_EXIT: &str = "exit";

/// ConditionType is a workflow condition type.
pub type ConditionType = String;

/// Signifies the workflow has completed.
pub const CONDITION_TYPE_COMPLETED: &str = "Completed";
/// Any workflow pods are currently running.
pub const CONDITION_TYPE_POD_RUNNING: &str = "PodRunning";
/// A warning on the current application spec.
pub const CONDITION_TYPE_SPEC_WARNING: &str = "SpecWarning";
/// An error on the current application spec.
pub const CONDITION_TYPE_SPEC_ERROR: &str = "SpecError";
/// An error during metric emission.
pub const CONDITION_TYPE_METRICS_ERROR: &str = "MetricsError";
/// An error on artifact garbage collection.
pub const CONDITION_TYPE_ARTIFACT_GC_ERROR: &str = "ArtifactGCError";

/// SynchronizationType is the type of a synchronization lock.
pub type SynchronizationType = String;

/// Synchronization types.
pub const SYNCHRONIZATION_TYPE_SEMAPHORE: &str = "Semaphore";
/// A mutex lock.
pub const SYNCHRONIZATION_TYPE_MUTEX: &str = "Mutex";
/// An unknown synchronization type.
pub const SYNCHRONIZATION_TYPE_UNKNOWN: &str = "Unknown";

/// ConcurrencyPolicy defines how to treat concurrent executions of a CronWorkflow.
pub type ConcurrencyPolicy = String;

/// Allow concurrent executions.
pub const CONCURRENCY_POLICY_ALLOW: &str = "Allow";
/// Forbid concurrent executions.
pub const CONCURRENCY_POLICY_FORBID: &str = "Forbid";
/// Replace concurrent executions.
pub const CONCURRENCY_POLICY_REPLACE: &str = "Replace";

/// CronWorkflowPhase is the phase of a CronWorkflow (v3.6+ stopStrategy).
pub type CronWorkflowPhase = String;

/// CronWorkflow is actively scheduling.
pub const CRON_WORKFLOW_PHASE_ACTIVE: &str = "Active";
/// CronWorkflow stopped scheduling (stopStrategy expression was true).
pub const CRON_WORKFLOW_PHASE_STOPPED: &str = "Stopped";

/// WorkflowPhase is the phase of a workflow (e.g. Running, Succeeded, Failed).
pub type WorkflowPhase = String;

/// Workflow phase unknown (zero value).
pub const WORKFLOW_UNKNOWN: &str = "";
/// Pending some set-up - rarely used.
pub const WORKFLOW_PENDING: &str = "Pending";
/// Any node has started; pods might not be running yet, may be suspended too.
pub const WORKFLOW_RUNNING: &str = "Running";
/// Workflow succeeded.
pub const WORKFLOW_SUCCEEDED: &str = "Succeeded";
/// Workflow failed (it may be that the workflow was terminated).
pub const WORKFLOW_FAILED: &str = "Failed";
/// Workflow errored.
pub const WORKFLOW_ERROR: &str = "Error";
