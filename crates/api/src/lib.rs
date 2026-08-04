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
//! - [`initpro`] — the `init-pro.io/v1` CRD types (e.g. `LuaRouter`).
#![forbid(unsafe_code)]

pub mod apply;
pub mod discovery;
pub mod gvk;
pub mod initpro;
pub mod patch;
pub mod schema;
pub mod serde_ext;

// Convenience re-exports so consumers depend on `api` only.
pub use apply::{apply_object, ApplyOptions, ApplyResult, Conflict, ManagedFieldEntry, Operation};
pub use discovery::{api_group_list, api_resource_list, core_api_versions};
pub use gvk::{ApiVersion, ApiVersionError, GroupVersionKind, GroupVersionResource};
pub use initpro::{LuaRouter, LuaRouterSpec};
pub use patch::{strategic_merge, PatchStrategy};
pub use schema::{SchemaRegistry, Scope, TypeInfo};
pub use serde_ext::{from_json, to_json, to_json_pretty};
