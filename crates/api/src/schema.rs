//! Schema registry: GVK -> type info (TODO **T1.1**, step S3).
//!
//! kube-core keeps `GroupVersionKind` and `GroupVersionResource` as separate
//! values and refuses to join them (the kind<->plural mapping is a runtime
//! concern owned by discovery). The schema registry is exactly that join:
//! it maps a GVK to the resource (plural) name, the list kind, and the scope,
//! letting us losslessly convert GVK<->GVR and answer REST-routing questions.
//!
//! Native core/v1 types are registered from their static `k8s_openapi::Resource`
//! consts (zero allocations at register time). `init-pro.io` CRD types (S6) are
//! registered with explicit fields.

use std::collections::BTreeMap;

use k8s_openapi::{ClusterResourceScope, ListableResource, NamespaceResourceScope, Resource};
use kube_core::gvk::{GroupVersionKind, GroupVersionResource};

use crate::gvk::gvr_from_gvk;

/// Resource scope (mirrors k8s `RESTScope`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Scope {
    Cluster,
    Namespaced,
}

impl Scope {
    pub fn is_namespaced(self) -> bool {
        matches!(self, Scope::Namespaced)
    }
}

/// Static type info stored per GVK.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeInfo {
    /// Singular CamelCase kind, e.g. `"Pod"`.
    pub kind: String,
    /// Plural / URL path segment, e.g. `"pods"`.
    pub resource: String,
    /// List kind, e.g. `"PodList"`.
    pub list_kind: String,
    pub scope: Scope,
}

impl TypeInfo {
    pub fn new(
        kind: impl Into<String>,
        resource: impl Into<String>,
        list_kind: impl Into<String>,
        scope: Scope,
    ) -> Self {
        Self {
            kind: kind.into(),
            resource: resource.into(),
            list_kind: list_kind.into(),
            scope,
        }
    }
}

/// Deduce [`Scope`] from a `k8s_openapi` scope marker type.
pub trait ScopeMarker {
    const SCOPE: Scope;
}
impl ScopeMarker for NamespaceResourceScope {
    const SCOPE: Scope = Scope::Namespaced;
}
impl ScopeMarker for ClusterResourceScope {
    const SCOPE: Scope = Scope::Cluster;
}

/// In-memory schema registry. Lookup by GVK or GVR is O(log n).
#[derive(Debug, Default, Clone)]
pub struct SchemaRegistry {
    /// Keyed by `(group, version, kind)`.
    by_gvk: BTreeMap<Key, TypeInfo>,
}

/// BTreeMap-friendly composite key (group, version, kind) — lowercased group
/// for case-insensitive lookup parity with upstream group normalization.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Key {
    group: String,
    version: String,
    kind: String,
}

impl Key {
    fn from_gvk(gvk: &GroupVersionKind) -> Self {
        Self {
            group: gvk.group.to_ascii_lowercase(),
            version: gvk.version.clone(),
            kind: gvk.kind.clone(),
        }
    }
}

impl SchemaRegistry {
    /// Empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Pre-populated with the core/v1 types init-pro serves in v1
    /// (Pod, ConfigMap, Secret, Service, Namespace, Node, Event).
    pub fn with_core_v1() -> Self {
        let mut reg = Self::new();
        reg.register_native::<k8s_openapi::api::core::v1::Pod>();
        reg.register_native::<k8s_openapi::api::core::v1::ConfigMap>();
        reg.register_native::<k8s_openapi::api::core::v1::Secret>();
        reg.register_native::<k8s_openapi::api::core::v1::Service>();
        reg.register_native::<k8s_openapi::api::core::v1::Namespace>();
        reg.register_native::<k8s_openapi::api::core::v1::Node>();
        reg.register_native::<k8s_openapi::api::core::v1::Event>();
        reg
    }

    /// Register a native `k8s_openapi` type from its static `Resource` consts.
    pub fn register_native<T>(&mut self)
    where
        T: Resource + ListableResource,
        T::Scope: ScopeMarker,
    {
        let group = <T as Resource>::GROUP.to_string();
        let version = <T as Resource>::VERSION.to_string();
        let kind = <T as Resource>::KIND.to_string();
        let info = TypeInfo::new(
            kind.clone(),
            <T as Resource>::URL_PATH_SEGMENT,
            <T as ListableResource>::LIST_KIND,
            T::Scope::SCOPE,
        );
        self.by_gvk.insert(
            Key {
                group: group.to_ascii_lowercase(),
                version,
                kind,
            },
            info,
        );
    }

    /// Register a CRD/custom type by explicit fields (used by init-pro.io groups).
    pub fn register(
        &mut self,
        group: &str,
        version: &str,
        kind: &str,
        resource: &str,
        list_kind: &str,
        scope: Scope,
    ) {
        self.by_gvk.insert(
            Key {
                group: group.to_ascii_lowercase(),
                version: version.to_string(),
                kind: kind.to_string(),
            },
            TypeInfo::new(kind, resource, list_kind, scope),
        );
    }

    /// Lookup by GVK. Group is matched case-insensitively (upstream parity).
    pub fn get(&self, gvk: &GroupVersionKind) -> Option<&TypeInfo> {
        self.by_gvk.get(&Key::from_gvk(gvk))
    }

    /// Lookup by GVR (group+version+resource/plural). Group is matched
    /// case-insensitively (upstream parity).
    pub fn get_by_gvr(&self, gvr: &GroupVersionResource) -> Option<&TypeInfo> {
        self.by_gvk
            .iter()
            .find(|(k, info)| {
                k.group.eq_ignore_ascii_case(&gvr.group)
                    && k.version == gvr.version
                    && info.resource == gvr.resource
            })
            .map(|(_, info)| info)
    }

    /// Convert GVK -> GVR using the registry's stored plural. Returns `None`
    /// if the GVK is unknown.
    pub fn gvr_for(&self, gvk: &GroupVersionKind) -> Option<GroupVersionResource> {
        self.get(gvk).map(|info| gvr_from_gvk(gvk, &info.resource))
    }

    /// Convert GVR -> GVK using the registry's stored kind. Returns `None`
    /// if the GVR is unknown.
    pub fn gvk_for(&self, gvr: &GroupVersionResource) -> Option<GroupVersionKind> {
        self.iter()
            .find(|(gvk, info)| {
                gvk.group.eq_ignore_ascii_case(&gvr.group)
                    && gvk.version == gvr.version
                    && info.resource == gvr.resource
            })
            .map(|(gvk, _)| gvk.clone())
    }

    /// Iterate all `(GVK, TypeInfo)` entries.
    pub fn iter(&self) -> impl Iterator<Item = (GroupVersionKind, &TypeInfo)> {
        self.by_gvk
            .iter()
            .map(|(k, info)| (GroupVersionKind::gvk(&k.group, &k.version, &k.kind), info))
    }

    /// Number of registered types.
    pub fn len(&self) -> usize {
        self.by_gvk.len()
    }

    /// Whether the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.by_gvk.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_v1_types_registered() {
        let reg = SchemaRegistry::with_core_v1();
        assert_eq!(reg.len(), 7);
        let pod = GroupVersionKind::gvk("", "v1", "Pod");
        let info = reg.get(&pod).unwrap();
        assert_eq!(info.kind, "Pod");
        assert_eq!(info.resource, "pods");
        assert_eq!(info.list_kind, "PodList");
        assert_eq!(info.scope, Scope::Namespaced);

        let ns = GroupVersionKind::gvk("", "v1", "Namespace");
        assert_eq!(reg.get(&ns).unwrap().scope, Scope::Cluster);
    }

    #[test]
    fn gvk_gvr_round_trip_through_registry_is_lossless() {
        let reg = SchemaRegistry::with_core_v1();
        for (gvk, _) in reg.iter() {
            let gvr = reg.gvr_for(&gvk).expect("gvr");
            let back = reg.gvk_for(&gvr).expect("gvk");
            assert_eq!(back, gvk, "GVK->GVR->GVK must be lossless");
        }
    }

    #[test]
    fn group_lookup_is_case_insensitive() {
        let reg = SchemaRegistry::with_core_v1();
        // core group is "" — case-insensitivity is a no-op here, but the
        // registered mixed-case kind must still resolve.
        assert!(reg
            .get(&GroupVersionKind::gvk("", "v1", "ConfigMap"))
            .is_some());
        assert!(reg
            .get(&GroupVersionKind::gvk("", "v1", "Missing"))
            .is_none());
    }

    #[test]
    fn custom_crd_registration() {
        let mut reg = SchemaRegistry::new();
        reg.register(
            "init-pro.io",
            "v1",
            "LuaRouter",
            "luarouters",
            "LuaRouterList",
            Scope::Namespaced,
        );
        let gvk = GroupVersionKind::gvk("init-pro.io", "v1", "LuaRouter");
        let info = reg.get(&gvk).unwrap();
        assert_eq!(info.resource, "luarouters");
        assert_eq!(info.scope, Scope::Namespaced);
        let gvr = reg.gvr_for(&gvk).unwrap();
        assert_eq!(
            gvr,
            GroupVersionResource::gvr("init-pro.io", "v1", "luarouters")
        );
        assert_eq!(reg.gvk_for(&gvr).unwrap(), gvk);
    }

    #[test]
    fn get_by_gvr_resolves_pod() {
        let reg = SchemaRegistry::with_core_v1();
        let gvr = GroupVersionResource::gvr("", "v1", "pods");
        let info = reg.get_by_gvr(&gvr).expect("pod GVR resolves");
        assert_eq!(info.kind, "Pod");
        assert_eq!(info.resource, "pods");
        assert_eq!(info.scope, Scope::Namespaced);
    }

    #[test]
    fn get_by_gvr_returns_none_for_unknown_resource() {
        let reg = SchemaRegistry::with_core_v1();
        let gvr = GroupVersionResource::gvr("", "v1", "nope");
        assert!(reg.get_by_gvr(&gvr).is_none());
    }

    #[test]
    fn get_by_gvr_matches_group_case_insensitively() {
        let mut reg = SchemaRegistry::new();
        reg.register(
            "init-pro.io",
            "v1",
            "LuaRouter",
            "luarouters",
            "LuaRouterList",
            Scope::Namespaced,
        );
        // Upstream group normalization is case-insensitive: an uppercased group
        // on the GVR must still resolve to the registered type.
        let gvr = GroupVersionResource::gvr("INIT-PRO.IO", "v1", "luarouters");
        let info = reg.get_by_gvr(&gvr).expect("case-insensitive group match");
        assert_eq!(info.kind, "LuaRouter");
    }

    #[test]
    fn is_empty_reflects_registration() {
        let mut reg = SchemaRegistry::new();
        assert!(reg.is_empty());
        reg.register_native::<k8s_openapi::api::core::v1::Pod>();
        assert!(!reg.is_empty());
        assert_eq!(reg.len(), 1);
    }
}
