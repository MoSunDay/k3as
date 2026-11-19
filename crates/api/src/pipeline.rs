//! `init-pro.io/v1` `ServicePipeline` — per-service CI/CD config (TODO
//! **T8.3**, decision **Q56**).
//!
//! One apply = one build: changing `spec.image.tag` (or any spec field)
//! triggers the controller to build/publish an image and patch the target
//! Deployment's container image. Canonical source is a docker build context
//! (`source.context` + `source.dockerfile`) or a prebuilt `docker save`
//! archive (`source.archive`, docker-free CI/e2e).
//!
//! `spec.release` optionally drives a canary rollout (T8.5, Q58): the
//! pipeline owns `<deployment>-canary` + the canary Service, steps traffic
//! weights, and promotes or aborts.

use kube_core::gvk::GroupVersionKind;
use kube_core::metadata::{ListMeta, ObjectMeta, TypeMeta};
use serde::{Deserialize, Serialize};

use crate::initpro::Condition;
use crate::schema::{SchemaRegistry, Scope};

/// API group + version (shared by every init-pro.io resource).
pub const GROUP: &str = "init-pro.io";
pub const VERSION: &str = "v1";
pub const API_VERSION: &str = "init-pro.io/v1";

/// Kind / plural / short name.
pub const KIND: &str = "ServicePipeline";
pub const PLURAL: &str = "servicepipelines";
pub const SHORT: &str = "sp";

/// `status.phase` values (Q56).
pub const PHASE_IDLE: &str = "Idle";
pub const PHASE_BUILDING: &str = "Building";
pub const PHASE_PUBLISHING: &str = "Publishing";
pub const PHASE_ROLLING: &str = "Rolling";
pub const PHASE_PROMOTED: &str = "Promoted";
pub const PHASE_ABORTED: &str = "Aborted";
pub const PHASE_FAILED: &str = "Failed";

/// `ServicePipeline` — one service's build + publish + release config.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct ServicePipeline {
    #[serde(flatten)]
    pub types: TypeMeta,
    pub metadata: ObjectMeta,
    pub spec: ServicePipelineSpec,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub status: Option<ServicePipelineStatus>,
}

/// Desired pipeline state.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ServicePipelineSpec {
    /// Where the image comes from (context or prebuilt archive).
    #[serde(default)]
    pub source: PipelineSource,
    /// Target image reference (`repository:tag`); a tag change triggers a build.
    #[serde(default)]
    pub image: PipelineImage,
    /// Build knobs (args, timeout).
    #[serde(default)]
    pub build: BuildOptions,
    /// Optional Deployment image patch after publishing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<PipelineTarget>,
    /// Optional canary release (T8.5, Q58).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release: Option<PipelineRelease>,
    /// Paused pipelines never build (`true` = hold the observed tag).
    #[serde(default, skip_serializing_if = "is_false")]
    pub paused: bool,
}

/// Image source: `context` (docker build) wins when non-empty, else `archive`.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PipelineSource {
    /// Docker build context directory (Q56: `docker build` on the host).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub context: String,
    /// Dockerfile path relative to `context` (default `Dockerfile`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub dockerfile: String,
    /// Prebuilt `docker save` archive (docker-free CI/e2e, Q56).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub archive: String,
}

impl PipelineSource {
    /// `true` when a docker build context is configured (takes precedence).
    pub fn is_context(&self) -> bool {
        !self.context.trim().is_empty()
    }

    /// `true` when a prebuilt archive is configured.
    pub fn is_archive(&self) -> bool {
        !self.archive.trim().is_empty()
    }

    /// The Dockerfile path to pass to `docker build -f` (empty = default).
    pub fn dockerfile_path(&self) -> Option<String> {
        let f = self.dockerfile.trim();
        (!f.is_empty()).then(|| {
            if f.starts_with('/') {
                f.to_string()
            } else {
                format!("{}/{}", self.context.trim_end_matches('/'), f)
            }
        })
    }
}

/// Target image reference.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PipelineImage {
    /// Registry repository, e.g. `192.168.31.57:15000/k3as/code-grapher`.
    #[serde(default)]
    pub repository: String,
    /// Image tag; changing it is the build trigger (Q56).
    #[serde(default)]
    pub tag: String,
}

impl PipelineImage {
    /// `repository:tag` (the pulled reference).
    pub fn reference(&self) -> String {
        format!("{}:{}", self.repository.trim_end_matches(':'), self.tag)
    }
}

/// Build knobs.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BuildOptions {
    /// Extra `docker build --build-arg` values (`KEY=VALUE`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Build timeout in seconds (0 = controller default).
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub timeout_seconds: u32,
}

/// Deployment image patch target.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PipelineTarget {
    /// Deployment name in the pipeline's namespace.
    #[serde(default)]
    pub deployment: String,
    /// Container name inside the Deployment's pod template.
    #[serde(default)]
    pub container: String,
}

/// Canary release config (T8.5, Q58).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PipelineRelease {
    /// `TrafficSplit` name the pipeline steps (usually == the entry Service).
    #[serde(default)]
    pub traffic_split: String,
    /// Stable backend Service (operator-owned, must exist).
    #[serde(default)]
    pub stable_service: String,
    /// Canary Service the pipeline owns (cloned from stable).
    #[serde(default)]
    pub canary_service: String,
    /// Canary Deployment the pipeline owns (cloned from stable).
    #[serde(default)]
    pub canary_deployment: String,
    /// Ordered weight steps; `pauseSeconds` is the soak time before the next.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub steps: Vec<ReleaseStep>,
    /// Abort the in-flight canary (`true` = delete the canary Deployment +
    /// Service, weight back to 0; the stable image is never touched).
    #[serde(default, skip_serializing_if = "is_false")]
    pub abort: bool,
}

/// One canary step.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ReleaseStep {
    /// Canary weight (0-100); the stable backend gets `100 - weight`.
    #[serde(default)]
    pub weight: i32,
    /// Seconds to wait at this weight before the next step (0 = immediate).
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub pause_seconds: u32,
}

/// Observed pipeline state.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ServicePipelineStatus {
    /// One of the `PHASE_*` constants.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub phase: String,
    /// The tag the controller last completed for this spec.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub observed_tag: String,
    /// Last build summary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_build: Option<BuildRef>,
    /// Canary progress while `phase == Rolling`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rollout: Option<RolloutStatus>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<Condition>,
}

/// A completed build summary (`status.lastBuild`).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BuildRef {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub image: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub digest: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub started_at: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub finished_at: String,
}

/// Canary rollout progress (`status.rollout`) incl. the image the current
/// canary revision runs (T8.5, Q58).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RolloutStatus {
    /// Zero-based index into `spec.release.steps`.
    #[serde(default)]
    pub step: i32,
    /// Current canary weight.
    #[serde(default)]
    pub weight: i32,
    /// Image reference the current canary revision runs.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub revision: String,
    /// RFC3339-ish timestamp the current weight was applied.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub since: String,
}

/// List wrapper.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct ServicePipelineList {
    #[serde(flatten)]
    pub types: TypeMeta,
    #[serde(default)]
    pub metadata: ListMeta,
    #[serde(default)]
    pub items: Vec<ServicePipeline>,
}

fn is_false(v: &bool) -> bool {
    !*v
}

fn is_zero_u32(v: &u32) -> bool {
    *v == 0
}

/// Register the `ServicePipeline` type (short name `sp`).
pub fn register(registry: &mut SchemaRegistry) {
    registry.register_with_short_names(
        GROUP,
        VERSION,
        KIND,
        PLURAL,
        "ServicePipelineList",
        Scope::Namespaced,
        &[SHORT],
    );
}

/// The GVK for `ServicePipeline` (convenience).
pub fn gvk() -> GroupVersionKind {
    GroupVersionKind::gvk(GROUP, VERSION, KIND)
}

/// Construct a `ServicePipeline` with correct `TypeMeta`.
pub fn new_pipeline(name: &str, namespace: &str, spec: ServicePipelineSpec) -> ServicePipeline {
    ServicePipeline {
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
        status: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::serde_ext::round_trip;

    fn pipeline() -> ServicePipeline {
        new_pipeline(
            "code-grapher",
            "default",
            ServicePipelineSpec {
                source: PipelineSource {
                    context: "/root/k3as-d/cg-build".into(),
                    dockerfile: "Dockerfile".into(),
                    archive: String::new(),
                },
                image: PipelineImage {
                    repository: "192.168.31.57:15000/k3as/code-grapher".into(),
                    tag: "build-42".into(),
                },
                build: BuildOptions {
                    args: vec!["MODE=release".into()],
                    timeout_seconds: 1800,
                },
                target: Some(PipelineTarget {
                    deployment: "code-grapher".into(),
                    container: "code-grapher".into(),
                }),
                release: Some(PipelineRelease {
                    traffic_split: "code-grapher".into(),
                    stable_service: "code-grapher-stable".into(),
                    canary_service: "code-grapher-canary".into(),
                    canary_deployment: "code-grapher-canary".into(),
                    steps: vec![
                        ReleaseStep {
                            weight: 10,
                            pause_seconds: 30,
                        },
                        ReleaseStep {
                            weight: 100,
                            pause_seconds: 0,
                        },
                    ],
                    abort: true,
                }),
                paused: false,
            },
        )
    }

    /// A populated `status.rollout` (T8.5, Q58) for the revision field.
    fn rollout_status() -> RolloutStatus {
        RolloutStatus {
            step: 1,
            weight: 10,
            revision: "reg.local:5000/team/web:v1".into(),
            since: "2026-11-19T00:00:00Z".into(),
        }
    }

    #[test]
    fn register_and_resolve_pipeline() {
        let mut reg = SchemaRegistry::new();
        register(&mut reg);
        let info = reg.get(&gvk()).expect("ServicePipeline registered");
        assert_eq!(info.kind, "ServicePipeline");
        assert_eq!(info.resource, "servicepipelines");
        assert!(info.short_names.iter().any(|s| s == "sp"));
    }

    #[test]
    fn round_trip_is_lossless() {
        let p = pipeline();
        assert_eq!(round_trip(&p).unwrap(), p);
        // T8.5: both new fields survive the JSON cycle — `spec.release.abort`
        // on the pipeline, `status.rollout.revision` on the rollout status.
        assert!(p.spec.release.as_ref().expect("release").abort);
        let rollout = rollout_status();
        let back: RolloutStatus = round_trip(&rollout).unwrap();
        assert_eq!(back, rollout);
        assert_eq!(back.revision, "reg.local:5000/team/web:v1");
    }

    #[test]
    fn image_reference_and_source_helpers() {
        let p = pipeline();
        assert_eq!(
            p.spec.image.reference(),
            "192.168.31.57:15000/k3as/code-grapher:build-42"
        );
        assert!(p.spec.source.is_context());
        assert!(!p.spec.source.is_archive());
        assert_eq!(
            p.spec.source.dockerfile_path().as_deref(),
            Some("/root/k3as-d/cg-build/Dockerfile")
        );
    }

    #[test]
    fn wire_shape_uses_camel_case() {
        let json = serde_json::to_string(&pipeline()).unwrap();
        assert!(json.contains("\"apiVersion\":\"init-pro.io/v1\""));
        assert!(json.contains("\"timeoutSeconds\":1800"));
        assert!(json.contains("\"pauseSeconds\":30"));
        assert!(json.contains("\"trafficSplit\":\"code-grapher\""));
        assert!(json.contains("\"abort\":true"));
        let rollout = serde_json::to_string(&rollout_status()).unwrap();
        assert!(rollout.contains("\"revision\":\"reg.local:5000/team/web:v1\""));
    }

    #[test]
    fn new_fields_are_omitted_when_unset() {
        // T8.5: `abort` defaults to false and `revision` to empty, so both stay
        // off the wire for a pipeline that never started a canary.
        let json = serde_json::to_string(&PipelineRelease::default()).unwrap();
        assert!(!json.contains("\"abort\""));
        let rollout = serde_json::to_string(&RolloutStatus::default()).unwrap();
        assert!(!rollout.contains("\"revision\""));
    }
}
