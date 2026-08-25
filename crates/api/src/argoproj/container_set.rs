//! Argo Workflows `ContainerSetTemplate` (TODO **T7.3 Scope A**, decision **Q30**).
//!
//! 1:1 typed port of upstream `container_set_template_types.go` v4 — the
//! field-level SSOT is the Go json tags; wire format is JSON-only (Q10).
//! Data-only port: the Go validation/`GetRetryStrategy` helpers are out of
//! scope. Embedded `corev1.Container` is reused from `k8s_openapi`.

use k8s_openapi::api::core::v1 as apiv1;
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use serde::{Deserialize, Serialize};

/// ContainerSetTemplate is a template subtype to enable multiple containers to
/// be added to a template to create a container set.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ContainerSetTemplate {
    /// Containers is the list of containers in the set.
    #[serde(default)]
    pub containers: Vec<ContainerNode>,
    /// VolumeMounts is a list of volume mounts applied to every container in
    /// the set.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub volume_mounts: Vec<apiv1::VolumeMount>,
    /// RetryStrategy describes how to retry container nodes if the container
    /// set fails. Note that this works differently from the template-level
    /// `retryStrategy` as it is a process-level retry that does not create
    /// new Pods or containers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_strategy: Option<ContainerSetRetryStrategy>,
}

/// ContainerSetRetryStrategy provides controls on how to retry a container set.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ContainerSetRetryStrategy {
    /// Duration is the time between each retry, examples values are "300ms",
    /// "1s" or "5m". Valid time units are "ns", "us" (or "µs"), "ms", "s",
    /// "m", "h".
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub duration: String,
    /// Retries is the maximum number of retry attempts for each container. It
    /// does not include the first, original attempt; the maximum number of
    /// total attempts will be `retries + 1`.
    ///
    /// Go pointer without `omitempty`: always serialized (`null` when unset),
    /// mirroring `encoding/json` behaviour for nil pointers.
    #[serde(default)]
    pub retries: Option<IntOrString>,
}

/// ContainerNode is a container node in a container set: a `corev1.Container`
/// (inlined on the wire, Go `json:",inline"`) plus the container-set
/// dependency list.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ContainerNode {
    /// Inlined upstream `corev1.Container` (Go: `Container \`json:",inline"\``).
    #[serde(flatten)]
    pub container: apiv1::Container,
    /// Dependencies are the container names this container depends on.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dependencies: Vec<String>,
}
