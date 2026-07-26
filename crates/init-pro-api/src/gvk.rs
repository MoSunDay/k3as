//! GVK / GVR / ApiVersion helpers (TODO **T1.1**, step S2).
//!
//! Thin wrappers over `kube_core::gvk` plus the bits kube-core deliberately
//! leaves to the caller: `ApiVersion` round-trip parsing and the
//! kind↔resource (singular↔plural) join that only a schema registry can know.
//!
//! Wire format is JSON-only for v1 (decision **Q10**); these types are the
//! serde-visible surface, never a protobuf codec.

use std::fmt;
use std::str::FromStr;

use kube_core::gvk::GroupVersion;
use kube_core::metadata::TypeMeta;
use thiserror::Error;

// Re-export so downstream crates depend on init-pro-api, not kube-core.
pub use kube_core::gvk::{GroupVersionKind, GroupVersionResource};

/// Error parsing an `apiVersion` string (e.g. `"apps/v1"` / `"v1"`).
#[derive(Debug, Error, PartialEq, Eq)]
#[error("invalid apiVersion `{raw}`: expected `<group>/<version>` or `<version>`")]
pub struct ApiVersionError {
    pub raw: String,
}

/// A parsed `apiVersion` — the `<group>/<version>` or bare `<version>` used in
/// every Kubernetes object's `TypeMeta.apiVersion`.
///
/// Round-trips with the wire string: `ApiVersion::from_str(s)` then
/// `to_string()` reproduces `s` exactly (empty group -> bare version).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ApiVersion {
    /// Empty for the core group (`""`), e.g. `"apps"` for `apps/v1`.
    pub group: String,
    /// e.g. `"v1"`, `"v1beta1"`.
    pub version: String,
}

impl ApiVersion {
    /// Build from explicit group + version.
    pub fn new(group: impl Into<String>, version: impl Into<String>) -> Self {
        Self { group: group.into(), version: version.into() }
    }

    /// Core (`""`) group shortcut.
    pub fn core(version: impl Into<String>) -> Self {
        Self { group: String::new(), version: version.into() }
    }

    /// `true` when the group is the core group (`""`).
    pub fn is_core(&self) -> bool {
        self.group.is_empty()
    }

    /// Wire form: `"v1"` for core, `"<group>/<version>"` otherwise.
    pub fn as_str(&self) -> String {
        if self.group.is_empty() {
            self.version.clone()
        } else {
            format!("{}/{}", self.group, self.version)
        }
    }
}

impl fmt::Display for ApiVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.as_str())
    }
}

impl FromStr for ApiVersion {
    type Err = ApiVersionError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.is_empty() {
            return Err(ApiVersionError { raw: s.to_string() });
        }
        // Delegate to kube-core's proven GroupVersion parser (handles the
        // core-vs-grouped split identically to upstream apimachinery).
        let gv = GroupVersion::from_str(s)
            .map_err(|_| ApiVersionError { raw: s.to_string() })?;
        // kube-core mirrors upstream `ParseGroupVersion`, which accepts a
        // trailing slash (`"apps/"` -> group "apps", version ""). An empty
        // version is never a usable apiVersion, so reject it here.
        if gv.version.is_empty() {
            return Err(ApiVersionError { raw: s.to_string() });
        }
        Ok(Self { group: gv.group, version: gv.version })
    }
}

impl From<&ApiVersion> for GroupVersion {
    fn from(av: &ApiVersion) -> Self {
        GroupVersion::gv(&av.group, &av.version)
    }
}

/// Derive a GVK from an `apiVersion` string + kind.
pub fn gvk_from_api_version(api_version: &str, kind: &str) -> Result<GroupVersionKind, ApiVersionError> {
    let av = ApiVersion::from_str(api_version)?;
    Ok(GroupVersionKind::gvk(&av.group, &av.version, kind))
}

/// Build a GVR from a GVK once the resource (plural) name is known.
///
/// The plural is what a schema registry supplies (S3); this helper is the
/// pure mechanical join so GVK→GVR is lossless given the registry's plural.
pub fn gvr_from_gvk(gvk: &GroupVersionKind, plural: &str) -> GroupVersionResource {
    GroupVersionResource::gvr(&gvk.group, &gvk.version, plural)
}

/// Recover a GVK from a GVR once the kind (singular, CamelCase) is known.
///
/// Mirrors `gvr_from_gvk`: the singular↔plural join is registry-owned.
pub fn gvk_from_gvr(gvr: &GroupVersionResource, kind: &str) -> GroupVersionKind {
    GroupVersionKind::gvk(&gvr.group, &gvr.version, kind)
}

/// Extract a GVK from a `TypeMeta` (`apiVersion` + `kind`), the standard
/// decode-time path. Returns `None` if either field is absent.
pub fn gvk_from_type_meta(tm: &TypeMeta) -> Option<GroupVersionKind> {
    if tm.api_version.is_empty() || tm.kind.is_empty() {
        return None;
    }
    gvk_from_api_version(&tm.api_version, &tm.kind).ok()
}

/// True when `gvr` is the core group (empty group string).
pub fn is_core_resource(gvr: &GroupVersionResource) -> bool {
    gvr.group.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_version_core_round_trips() {
        let av = ApiVersion::from_str("v1").unwrap();
        assert_eq!(av.group, "");
        assert_eq!(av.version, "v1");
        assert!(av.is_core());
        assert_eq!(av.to_string(), "v1");
    }

    #[test]
    fn api_version_grouped_round_trips() {
        let av = ApiVersion::from_str("apps/v1").unwrap();
        assert_eq!(av.group, "apps");
        assert_eq!(av.version, "v1");
        assert!(!av.is_core());
        assert_eq!(av.to_string(), "apps/v1");
    }

    #[test]
    fn api_version_rejects_empty_and_garbage() {
        assert_eq!(ApiVersion::from_str("").unwrap_err().raw, "");
        // A bare group with no version is invalid apimachinery form.
        assert!(ApiVersion::from_str("apps/").is_err());
    }

    #[test]
    fn gvk_to_gvr_to_gvk_is_lossless_with_registry_plural() {
        let gvk = GroupVersionKind::gvk("apps", "v1", "Deployment");
        let plural = "deployments";
        let gvr = gvr_from_gvk(&gvk, plural);
        assert_eq!(gvr.resource, plural);
        let back = gvk_from_gvr(&gvr, "Deployment");
        assert_eq!(back, gvk);
    }

    #[test]
    fn gvk_from_api_version_core_and_grouped() {
        let core = gvk_from_api_version("v1", "Pod").unwrap();
        assert_eq!(core, GroupVersionKind::gvk("", "v1", "Pod"));
        let grp = gvk_from_api_version("rbac.authorization.k8s.io/v1", "Role").unwrap();
        assert_eq!(grp, GroupVersionKind::gvk("rbac.authorization.k8s.io", "v1", "Role"));
    }

    #[test]
    fn gvk_from_type_meta_round_trips() {
        let tm = TypeMeta { api_version: "batch/v1".into(), kind: "Job".into() };
        let gvk = gvk_from_type_meta(&tm).unwrap();
        assert_eq!(gvk, GroupVersionKind::gvk("batch", "v1", "Job"));
        assert!(gvk_from_type_meta(&TypeMeta::default()).is_none());
    }

    #[test]
    fn kube_core_gvk_api_version_matches() {
        // Sanity: kube-core's own api_version() agrees with our ApiVersion.
        let gvk = GroupVersionKind::gvk("networking.k8s.io", "v1", "Ingress");
        let av = ApiVersion::new("networking.k8s.io", "v1");
        assert_eq!(gvk.api_version(), av.to_string());
        let _ = GroupVersionResource::gvr("networking.k8s.io", "v1", "ingresses"); // smoke
    }


    #[test]
    fn is_core_resource_distinguishes_core_from_grouped() {
        let core = GroupVersionResource::gvr("", "v1", "pods");
        assert!(is_core_resource(&core));
        let grouped = GroupVersionResource::gvr("apps", "v1", "deployments");
        assert!(!is_core_resource(&grouped));
    }

}
