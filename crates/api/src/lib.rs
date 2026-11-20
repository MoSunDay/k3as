//! Kubernetes-faithful resource model for init-pro (TODO **T1.1**).
//!
//! Built on `kube-core` + `k8s-openapi`. The wire format is **JSON-only for
//! v1** (decision **Q10**): there is no protobuf codec — serde JSON is the
//! sole transport for the API server, etcd storage, and watch streams.
//!
//! # Modules
//!
//! - [`gvk`] — `ApiVersion` parsing + GVK/GVR join helpers.
//! - [`schema`] — `SchemaRegistry`: GVK → type info (kind/plural/list/scope).
//! - [`serde_ext`] — round-trip-faithful JSON (de)serialization helpers.
//! - [`patch`] — Strategic Merge Patch (core/v1 strategies) + JSON Patch fallback.
//! - [`apply`] — Server-Side Apply field-manager (T1.2c).
//! - [`agent`] — the `init-pro.io/v1` `Agent` resource (T7.1 rewrite, Q35).
//! - [`initpro`] — the `init-pro.io/v1` CRD types (e.g. `LuaRouter`).
//! - [`traffic`] — `TrafficSplit`: weighted one-click traffic switching
//!   consumed by the Router's NodePort plane (T8.2, Q57).
//! - [`pipeline`] / [`build`] — `ServicePipeline` + `Build`: per-service
//!   CI/CD config and immutable build records (T8.3, Q56).
//! - [`networking`] — the `networking.k8s.io/v1` group (Ingress/IngressClass;
//!   consumed by the openrusty Ingress gateway, decision Q32).
//! - [`validate`] — admission validation for Pod specs/templates: explicit
//!   `imagePullPolicy`, whole-GPU quantities, GPU visibility-env conflict,
//!   and the shared host-direct rules (T4.15/T7.2, Q60/Q61).
#![forbid(unsafe_code)]

pub mod agent;
pub mod apply;
pub mod argoproj;
pub mod build;
pub mod discovery;
pub mod gvk;
pub mod initpro;
pub mod networking;
pub mod patch;
pub mod pipeline;
pub mod schema;
pub mod serde_ext;
pub mod traffic;
pub mod validate;
pub mod validate_host_direct;

// Convenience re-exports so consumers depend on `api` only.
pub use agent::{
    Agent, AgentCondition, AgentList, AgentSpec, AgentStatus, KIND as AGENT_KIND,
    PLURAL as AGENT_PLURAL,
};
pub use apply::{apply_object, ApplyOptions, ApplyResult, Conflict, ManagedFieldEntry, Operation};
pub use build::{Build, BuildList, BuildSpec, BuildStatus};
pub use discovery::{api_group_list, api_resource_list, core_api_versions};
pub use gvk::{ApiVersion, ApiVersionError, GroupVersionKind, GroupVersionResource};
pub use initpro::{LuaRouter, LuaRouterSpec};
pub use networking::{
    HTTPIngressPath, HTTPIngressRuleValue, Ingress, IngressBackend, IngressClass, IngressClassList,
    IngressClassSpec, IngressList, IngressLoadBalancerIngress, IngressLoadBalancerStatus,
    IngressRule, IngressServiceBackend, IngressSpec, IngressStatus, IngressTLS, ServiceBackendPort,
    TypedLocalObjectReference, API_VERSION as NETWORKING_API_VERSION, GROUP as NETWORKING_GROUP,
    VERSION as NETWORKING_VERSION,
};
pub use patch::{strategic_merge, PatchStrategy};
pub use pipeline::{
    BuildOptions, BuildRef, PipelineImage, PipelineRelease, PipelineSource, PipelineTarget,
    ReleaseStep, RolloutStatus, ServicePipeline, ServicePipelineList, ServicePipelineSpec,
    ServicePipelineStatus,
};
pub use schema::{SchemaRegistry, Scope, TypeInfo};
pub use serde_ext::{from_json, to_json, to_json_pretty};
pub use traffic::{
    SplitBackend, TrafficSplit, TrafficSplitList, TrafficSplitSpec, TrafficSplitStatus,
};
pub use validate::{admit, HOST_DIRECT_ROOT, ROOTFS_ANNOTATION};
