//! `init-pro.io` API group types (TODO **T1.1**, step S6).
//!
//! These are init-pro's own CRDs (the Router configuration surface). They are
//! first-class Kubernetes objects: flattened `TypeMeta`, `ObjectMeta`, a typed
//! spec, and an optional status — serde-compatible with `kubectl apply` and
//! `kube-rs`. The `init-pro.io/v1` group is the stable home for router config;
//! see decision **Q4** (the built-in Router).

use kube_core::gvk::GroupVersionKind;
use kube_core::metadata::{ObjectMeta, TypeMeta};
use serde::{Deserialize, Serialize};

use crate::schema::{Scope, SchemaRegistry};

/// API group + version for init-pro's own resources.
pub const GROUP: &str = "init-pro.io";
pub const VERSION: &str = "v1";
pub const API_VERSION: &str = "init-pro.io/v1";

/// `LuaRouter` — declares a set of HTTP routes whose handlers are Lua programs
/// (the Router data-plane DSL, Q4). This is the primary router-config object.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct LuaRouter {
    #[serde(flatten)]
    pub types: TypeMeta,
    pub metadata: ObjectMeta,
    pub spec: LuaRouterSpec,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub status: Option<LuaRouterStatus>,
}

/// The desired router configuration.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct LuaRouterSpec {
    /// Ordered route rules; first match wins (priority by list order, or
    /// explicit `priority` descending when present).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub routes: Vec<LuaRoute>,
    /// Lua source executed in `access_by_lua` for every request before routing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access_handler: Option<String>,
}

/// A single route rule.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct LuaRoute {
    pub name: String,
    /// Host matcher (empty = any host).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub host: String,
    /// Path prefix matcher (empty = any path).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub path_prefix: String,
    /// Lua handler source (`content_by_lua`).
    pub handler: String,
    /// Higher priority wins; 0 = list order.
    #[serde(default, skip_serializing_if = "is_zero_i32")]
    pub priority: i32,
}

/// Observed router state.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct LuaRouterStatus {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<Condition>,
    /// Number of routes compiled into the data plane.
    #[serde(default, skip_serializing_if = "is_zero_i32")]
    pub active_routes: i32,
}

/// A minimal condition (subset of `meta/v1.Condition`) for status reporting.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct Condition {
    #[serde(rename = "type")]
    pub type_: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reason: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub message: String,
}

fn is_zero_i32(v: &i32) -> bool {
    *v == 0
}

/// Register the `init-pro.io/v1` types into a [`SchemaRegistry`].
pub fn register(registry: &mut SchemaRegistry) {
    registry.register(
        GROUP,
        VERSION,
        "LuaRouter",
        "luarouters",
        "LuaRouterList",
        Scope::Namespaced,
    );
}

/// The GVK for `LuaRouter` (convenience).
pub fn gvk() -> GroupVersionKind {
    GroupVersionKind::gvk(GROUP, VERSION, "LuaRouter")
}

/// Construct a `LuaRouter` with correct `TypeMeta`.
pub fn new_lua_router(name: &str, namespace: &str, spec: LuaRouterSpec) -> LuaRouter {
    LuaRouter {
        types: TypeMeta { api_version: API_VERSION.to_string(), kind: "LuaRouter".to_string() },
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
    use crate::serde_ext::{from_json, round_trip, to_json};

    #[test]
    fn register_and_resolve_luarouter() {
        let mut reg = SchemaRegistry::new();
        register(&mut reg);
        let info = reg.get(&gvk()).expect("LuaRouter registered");
        assert_eq!(info.kind, "LuaRouter");
        assert_eq!(info.resource, "luarouters");
        assert_eq!(info.list_kind, "LuaRouterList");
        assert_eq!(info.scope, Scope::Namespaced);
    }

    #[test]
    fn luarouter_round_trip_is_lossless() {
        let r = new_lua_router(
            "edge",
            "default",
            LuaRouterSpec {
                routes: vec![LuaRoute {
                    name: "api".into(),
                    host: "api.example.com".into(),
                    path_prefix: "/v1".into(),
                    handler: "ngx.say('hello')".into(),
                    priority: 10,
                }],
                access_handler: Some("local x = 1".into()),
            },
        );
        let back = round_trip(&r).unwrap();
        assert_eq!(back, r);
    }

    #[test]
    fn luarouter_wire_json_has_camel_case_keys() {
        let r = new_lua_router("a", "ns", LuaRouterSpec::default());
        let bytes = to_json(&r).unwrap();
        let s = String::from_utf8(bytes).unwrap();
        assert!(s.contains("\"apiVersion\":\"init-pro.io/v1\""));
        assert!(s.contains("\"kind\":\"LuaRouter\""));
        assert!(s.contains("\"pathPrefix\"") || !s.contains("path_prefix"));
        // status omitted when None
        assert!(!s.contains("\"status\""));
    }

    #[test]
    fn luarouter_decodes_from_kubectl_apply_json() {
        let json = r#"{
            "apiVersion": "init-pro.io/v1",
            "kind": "LuaRouter",
            "metadata": {"name": "edge", "namespace": "default"},
            "spec": {
                "routes": [
                    {"name": "root", "pathPrefix": "/", "handler": "ngx.exit(200)"}
                ]
            }
        }"#;
        let lr: LuaRouter = from_json(json.as_bytes()).unwrap();
        assert_eq!(lr.metadata.name.as_deref(), Some("edge"));
        assert_eq!(lr.spec.routes.len(), 1);
        assert_eq!(lr.spec.routes[0].handler, "ngx.exit(200)");
    }
}
