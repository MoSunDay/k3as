//! `init-pro.io/v1` `Build` — one immutable build record per pipeline tag
//! (TODO **T8.3**, decision **Q56**).
//!
//! The controller creates a `Build` named `<pipeline>-<tag>` for every tag
//! transition, runs the executor, and records the outcome in `status`
//! (`phase`, `digest`, `logTail`). Spec is immutable; `OwnerReferences`
//! point at the owning `ServicePipeline` so GC cleans history with it.
//!
//! No `/status` subresource exists in v1 (Q10/Q56): the controller writes
//! status via full-object CAS (`Client::update(if_revision)`).

use kube_core::gvk::GroupVersionKind;
use kube_core::metadata::{ListMeta, ObjectMeta, TypeMeta};
use serde::{Deserialize, Serialize};

use crate::initpro::Condition;
use crate::pipeline::{PipelineImage, PipelineSource};
use crate::schema::{SchemaRegistry, Scope};

/// API group + version (shared by every init-pro.io resource).
pub const GROUP: &str = "init-pro.io";
pub const VERSION: &str = "v1";
pub const API_VERSION: &str = "init-pro.io/v1";

/// Kind / plural / short name.
pub const KIND: &str = "Build";
pub const PLURAL: &str = "builds";
pub const SHORT: &str = "bld";

/// `status.phase` values (Pod-style terminal semantics).
pub const PHASE_PENDING: &str = "Pending";
pub const PHASE_BUILDING: &str = "Building";
pub const PHASE_PUBLISHING: &str = "Publishing";
pub const PHASE_SUCCEEDED: &str = "Succeeded";
pub const PHASE_FAILED: &str = "Failed";

/// `Build` — one immutable build request.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Build {
    #[serde(flatten)]
    pub types: TypeMeta,
    pub metadata: ObjectMeta,
    pub spec: BuildSpec,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub status: Option<BuildStatus>,
}

/// Immutable build spec (captured from the pipeline at trigger time).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct BuildSpec {
    /// Owning `ServicePipeline` name.
    #[serde(default)]
    pub pipeline: String,
    /// Image to publish.
    #[serde(default)]
    pub image: PipelineImage,
    /// Source (context / archive) captured at trigger time.
    #[serde(default)]
    pub source: PipelineSource,
    /// Extra `docker build` args.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Build timeout in seconds (0 = controller default).
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub timeout_seconds: u32,
}

/// Observed build state.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BuildStatus {
    /// One of the `PHASE_*` constants.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub phase: String,
    /// Manifest digest of the published image (`sha256:...`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub digest: String,
    /// Pullable reference (`repository:tag`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub image: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub started_at: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub finished_at: String,
    /// Tail of the build/publish log (bounded by the executor).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub log_tail: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<Condition>,
}

/// List wrapper.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct BuildList {
    #[serde(flatten)]
    pub types: TypeMeta,
    #[serde(default)]
    pub metadata: ListMeta,
    #[serde(default)]
    pub items: Vec<Build>,
}

fn is_zero_u32(v: &u32) -> bool {
    *v == 0
}

impl Build {
    /// Terminal phase? (`Succeeded` / `Failed` builds are never re-run.)
    pub fn is_terminal(&self) -> bool {
        self.status
            .as_ref()
            .is_some_and(|s| matches!(s.phase.as_str(), PHASE_SUCCEEDED | PHASE_FAILED))
    }
}

/// Register the `Build` type (short name `bld`).
pub fn register(registry: &mut SchemaRegistry) {
    registry.register_with_short_names(
        GROUP,
        VERSION,
        KIND,
        PLURAL,
        "BuildList",
        Scope::Namespaced,
        &[SHORT],
    );
}

/// The GVK for `Build` (convenience).
pub fn gvk() -> GroupVersionKind {
    GroupVersionKind::gvk(GROUP, VERSION, KIND)
}

/// Construct a `Build` with correct `TypeMeta` and a `Pending` status.
pub fn new_build(name: &str, namespace: &str, spec: BuildSpec) -> Build {
    Build {
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
        status: Some(BuildStatus {
            phase: PHASE_PENDING.to_string(),
            ..BuildStatus::default()
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::serde_ext::round_trip;

    fn build() -> Build {
        new_build(
            "code-grapher-build-42",
            "default",
            BuildSpec {
                pipeline: "code-grapher".into(),
                image: PipelineImage {
                    repository: "192.168.31.57:15000/k3as/code-grapher".into(),
                    tag: "build-42".into(),
                },
                source: PipelineSource {
                    archive: "/tmp/cg.tar".into(),
                    ..PipelineSource::default()
                },
                args: Vec::new(),
                timeout_seconds: 1800,
            },
        )
    }

    #[test]
    fn register_and_resolve_build() {
        let mut reg = SchemaRegistry::new();
        register(&mut reg);
        let info = reg.get(&gvk()).expect("Build registered");
        assert_eq!(info.kind, "Build");
        assert_eq!(info.resource, "builds");
        assert!(info.short_names.iter().any(|s| s == "bld"));
    }

    #[test]
    fn round_trip_is_lossless() {
        let b = build();
        assert_eq!(round_trip(&b).unwrap(), b);
    }

    #[test]
    fn pending_build_is_not_terminal() {
        let mut b = build();
        assert!(!b.is_terminal());
        b.status.as_mut().unwrap().phase = PHASE_SUCCEEDED.into();
        assert!(b.is_terminal());
        b.status.as_mut().unwrap().phase = PHASE_FAILED.into();
        assert!(b.is_terminal());
    }

    #[test]
    fn wire_shape_carries_pipeline_and_image() {
        let json = serde_json::to_string(&build()).unwrap();
        assert!(json.contains("\"kind\":\"Build\""));
        assert!(json.contains("\"pipeline\":\"code-grapher\""));
        assert!(json.contains("\"archive\":\"/tmp/cg.tar\""));
        assert!(json.contains("\"timeoutSeconds\":1800"));
    }
}
