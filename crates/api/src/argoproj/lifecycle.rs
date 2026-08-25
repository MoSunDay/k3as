//! Argo Workflows lifecycle types: GC, hooks, synchronization, retry and
//! memoization (TODO **T7.3 Scope A**, decision **Q30**).
//!
//! 1:1 typed port of the `PodGC`/`WorkflowLevelArtifactGC`/`ArtifactGC`/
//! `VolumeClaimGC`/`LifecycleHook(s)`/`Synchronization`/`RetryStrategy`/
//! `Memoize` blocks of upstream `workflow_types.go` v4. Field-level SSOT is
//! the Go json tags; wire format is JSON-only (decision **Q10**). Data-only
//! port: Go helper methods (`GetStrategy`, `RetryPolicyActual`,
//! `WithArgs`, ...) are not carried. Note: this upstream v4 source has no
//! `ExpressionLanguage` type or `retryStrategy.expressionLanguage` field.

use std::collections::BTreeMap;

use k8s_openapi::api::core::v1 as apiv1;
use k8s_openapi::apimachinery::pkg::apis::meta::v1 as metav1;
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use serde::{Deserialize, Serialize};

use super::dag::TemplateRef;
use super::io::Arguments;
use super::is_false;
use super::phases::{ArtifactGCStrategy, PodGCStrategy, RetryPolicy, VolumeClaimGCStrategy};
use super::template::Metadata;

/// PodGC describes how to delete completed pods. Strategy is one of
/// `OnPodCompletion`, `OnPodSuccess`, `OnWorkflowCompletion`,
/// `OnWorkflowSuccess`; if unset, pods are not deleted.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PodGC {
    /// Strategy is the strategy to use.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub strategy: PodGCStrategy,
    /// LabelSelector to check if the pods match the labels before being
    /// added to the pod GC queue.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label_selector: Option<metav1::LabelSelector>,
    /// DeleteDelayDuration specifies the duration before pods in the GC
    /// queue get deleted.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub delete_delay_duration: String,
}

/// ArtifactGC describes how to delete artifacts from completed Workflows —
/// embedded into [`WorkflowLevelArtifactGC`] and also used on individual
/// artifacts to override the workflow-level setting.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactGC {
    /// Strategy is the strategy to use (`OnWorkflowCompletion`,
    /// `OnWorkflowDeletion`, `Never`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub strategy: ArtifactGCStrategy,
    /// PodMetadata optionally specifies the Labels and Annotations that
    /// should be assigned to the Pod doing the deletion.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pod_metadata: Option<Metadata>,
    /// ServiceAccountName optionally specifies the Service Account that
    /// should be assigned to the Pod doing the deletion.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub service_account_name: String,
}

/// WorkflowLevelArtifactGC describes how to delete artifacts from completed
/// Workflows — this spec is used on the Workflow level. The embedded Go
/// `ArtifactGC` (`json:",inline"`) is flattened onto the wire via
/// `#[serde(flatten)]`.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowLevelArtifactGC {
    /// ArtifactGC is an embedded struct (strategy / podMetadata /
    /// serviceAccountName).
    #[serde(flatten)]
    pub artifact_gc: ArtifactGC,
    /// ForceFinalizerRemoval: if set to true, the finalizer will be removed
    /// in the case that Artifact GC fails.
    #[serde(default, skip_serializing_if = "is_false")]
    pub force_finalizer_removal: bool,
    /// PodSpecPatch holds a strategic merge patch to apply against the
    /// artifact-GC pod spec.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub pod_spec_patch: String,
}

/// VolumeClaimGC describes how to delete volumes from completed Workflows.
/// Strategy is `OnWorkflowCompletion` or `OnWorkflowSuccess` (default).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct VolumeClaimGC {
    /// Strategy is the strategy to use.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub strategy: VolumeClaimGCStrategy,
}

/// LifecycleHooks maps a lifecycle event (e.g. `exit`) to a
/// [`LifecycleHook`] (Go: `map[LifecycleEvent]LifecycleHook`;
/// [`super::phases::LifecycleEvent`] is a transparent `String` alias).
pub type LifecycleHooks = BTreeMap<String, LifecycleHook>;

/// LifecycleHook is a template to execute on a lifecycle event of a step,
/// DAG task or the workflow itself.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct LifecycleHook {
    /// Template is the name of the template to execute by the hook.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub template: String,
    /// Arguments hold arguments to the template. (Go puts `omitempty` on a
    /// non-pointer struct — a no-op there, so it is always serialized.)
    #[serde(default)]
    pub arguments: Arguments,
    /// TemplateRef is the reference to the template resource to execute by
    /// the hook.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template_ref: Option<TemplateRef>,
    /// Expression is a condition expression for when a node will be retried.
    /// If it evaluates to false, the node will not be retried and the retry
    /// strategy will be ignored.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub expression: String,
}

/// Synchronization holds synchronization lock configuration.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Synchronization {
    /// Semaphores holds the list of Semaphore configurations (v3.6+).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub semaphores: Vec<SemaphoreRef>,
    /// Mutexes holds the list of Mutex lock details (v3.6+).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mutexes: Vec<Mutex>,
}

/// SyncDatabaseRef references a key in the synchronization database.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SyncDatabaseRef {
    /// Key of the synchronization database entry.
    #[serde(default)]
    pub key: String,
}

/// SemaphoreRef is a reference to a Semaphore.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SemaphoreRef {
    /// ConfigMapKeyRef is a configmap selector for Semaphore configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_map_key_ref: Option<apiv1::ConfigMapKeySelector>,
    /// Namespace of the configmap; defaults to the workflow's namespace.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub namespace: String,
    /// Database reference for Semaphore configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database: Option<SyncDatabaseRef>,
}

/// Mutex holds Mutex configuration.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Mutex {
    /// Name of the mutex.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    /// Namespace of the mutex; defaults to the workflow's namespace.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub namespace: String,
    /// Database specifies this mutex is database controlled if set true.
    #[serde(default, skip_serializing_if = "is_false")]
    pub database: bool,
}

/// RetryStrategy provides controls on how to retry a workflow step.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RetryStrategy {
    /// Limit is the maximum number of retry attempts when retrying a
    /// container; total attempts are `limit + 1` (the original not included).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<IntOrString>,
    /// RetryPolicy is a policy of node statuses that will be retried
    /// (`Always`, `OnFailure`, `OnError`, `OnTransientError`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub retry_policy: RetryPolicy,
    /// Backoff is a backoff strategy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backoff: Option<Backoff>,
    /// Affinity prevents running workflow's step on the same host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub affinity: Option<RetryAffinity>,
    /// Expression is a condition expression for when a node will be retried.
    /// If it evaluates to false, the node will not be retried and the retry
    /// strategy will be ignored.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub expression: String,
}

/// Backoff is a backoff strategy to use within a retry strategy.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Backoff {
    /// Duration is the amount to back off. Default unit is seconds, but it
    /// could also be a duration (e.g. "2m", "1h").
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub duration: String,
    /// Factor is a factor to multiply the base duration after each failed
    /// retry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub factor: Option<IntOrString>,
    /// MaxDuration is the maximum amount of time allowed for the backoff
    /// strategy. If the template includes activeDeadlineSeconds, a failed
    /// pod's deadline is overridden by maxDuration so retries do not exceed
    /// it.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub max_duration: String,
    /// Cap is a limit on revised values of the duration parameter. If a
    /// multiplication by `factor` would make the duration exceed the cap,
    /// the duration is set to the cap.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub cap: String,
}

/// RetryNodeAntiAffinity is a placeholder for future expansion, only an
/// empty nodeAntiAffinity is allowed. It prevents running steps on the same
/// host via "kubernetes.io/hostname". (Go empty struct — serializes as
/// `{}`.)
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RetryNodeAntiAffinity {}

/// RetryAffinity prevents running steps on the same host.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RetryAffinity {
    /// NodeAntiAffinity is the (currently only, empty) anti-affinity
    /// configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_anti_affinity: Option<RetryNodeAntiAffinity>,
}

/// Memoize enables caching for the Outputs of the template.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Memoize {
    /// Key to use as the caching key.
    #[serde(default)]
    pub key: String,
    /// Cache sets and configures the kind of cache. (Go has no `omitempty`
    /// on this pointer, so `null` is emitted when unset.)
    #[serde(default)]
    pub cache: Option<Cache>,
    /// MaxAge is the maximum age (e.g. "180s", "24h") of an entry that is
    /// still considered valid; older entries are ignored.
    #[serde(default)]
    pub max_age: String,
}

/// MemoizationStatus is the status of a memoized node.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MemoizationStatus {
    /// Hit indicates whether this node was created from a cache entry.
    #[serde(default)]
    pub hit: bool,
    /// Key is the name of the key used for this node's cache.
    #[serde(default)]
    pub key: String,
    /// Cache is the name of the cache that was used.
    #[serde(default)]
    pub cache_name: String,
}

/// Cache is the configuration for the type of cache to be used.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Cache {
    /// ConfigMap sets a ConfigMap-based cache. (Go has no `omitempty` on
    /// this pointer, so `null` is emitted when unset.)
    #[serde(default)]
    pub config_map: Option<apiv1::LocalObjectReference>,
}
