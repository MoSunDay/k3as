//! Shared application state + REST routing helpers (TODO **T1.2b**).
//!
//! [`AppState`] carries the served schema + a [`StorageBackend`] and is the
//! single shared state cloned across all axum handler tasks. The helpers here
//! resolve a REST `(group, version, resource)` triple against the schema and
//! shape stored JSON objects with Kubernetes `resourceVersion` bookkeeping:
//! storage owns the revision; the API layer projects `mod_revision` into
//! `metadata.resourceVersion` on the wire (decision **Q10** — JSON-only).

use std::sync::Arc;

use api::{GroupVersionResource, SchemaRegistry, Scope};
use serde_json::Value;
use storage::{Key, KeyPrefix, StorageBackend};

/// Shared, immutable application state: served schema + storage + the
/// advertised server address. Cloned cheaply (inner `Arc`s).
#[derive(Clone)]
pub(crate) struct AppState {
    pub registry: Arc<SchemaRegistry>,
    pub store: Arc<dyn StorageBackend>,
    pub server_address: String,
}

/// A resource resolved from the schema for one REST request.
pub(crate) struct Resolved {
    /// Wire apiVersion, e.g. `"v1"` (core) or `"apps/v1"` (grouped).
    pub api_version: String,
    /// CamelCase kind, e.g. `"ConfigMap"`.
    pub kind: String,
    /// List kind, e.g. `"ConfigMapList"`.
    pub list_kind: String,
    /// Resource scope (namespaced vs cluster).
    pub scope: Scope,
}

/// A REST request location: group/version/resource + optional namespace.
/// `group` is empty for the core (`""`) group.
pub(crate) struct Loc {
    pub group: String,
    pub version: String,
    pub resource: String,
    /// `Some` when the path carried `/namespaces/<ns>/...`.
    pub namespace: Option<String>,
}
impl Loc {
    pub(crate) fn new(
        group: &str,
        version: &str,
        resource: String,
        namespace: Option<String>,
    ) -> Self {
        Self {
            group: group.to_string(),
            version: version.to_string(),
            resource,
            namespace,
        }
    }
}

/// Resolve `(group, version, resource)` -> [`Resolved`]. Returns `None` when
/// the resource is not served (upstream answers 404 for unknown resources).
pub(crate) fn resolve(
    reg: &SchemaRegistry,
    group: &str,
    version: &str,
    resource: &str,
) -> Option<Resolved> {
    let gvr = GroupVersionResource::gvr(group, version, resource);
    let info = reg.get_by_gvr(&gvr)?;
    let api_version = if group.is_empty() {
        version.to_string()
    } else {
        format!("{group}/{version}")
    };
    Some(Resolved {
        api_version,
        kind: info.kind.clone(),
        list_kind: info.list_kind.clone(),
        scope: info.scope,
    })
}

/// Storage key for one namespaced object.
pub(crate) fn namespaced_key(group: &str, resource: &str, namespace: &str, name: &str) -> Key {
    Key::new(group, resource, namespace, name)
}

/// Storage key for one cluster-scoped object (empty namespace).
pub(crate) fn cluster_key(group: &str, resource: &str, name: &str) -> Key {
    Key::new(group, resource, "", name)
}

/// Collection prefix for a list/watch. `namespace = None` = all namespaces.
pub(crate) fn collection_prefix(group: &str, resource: &str, namespace: Option<&str>) -> KeyPrefix {
    KeyPrefix::new(group, resource, namespace.map(str::to_string))
}

/// Set `metadata.namespace` on an object (before persistence).
pub(crate) fn set_namespace(value: &mut Value, namespace: &str) {
    if let Some(obj) = value.as_object_mut() {
        let meta = obj
            .entry("metadata")
            .or_insert_with(|| Value::Object(serde_json::Map::new()));
        if let Some(m) = meta.as_object_mut() {
            m.insert("namespace".into(), Value::String(namespace.to_string()));
        }
    }
}

/// Set `apiVersion` + `kind` on a stored object (before persistence).
pub(crate) fn set_type_meta(value: &mut Value, api_version: &str, kind: &str) {
    if let Some(obj) = value.as_object_mut() {
        obj.insert("apiVersion".into(), Value::String(api_version.to_string()));
        obj.insert("kind".into(), Value::String(kind.to_string()));
    }
}

/// Project `mod_revision` into `metadata.resourceVersion` (the wire field).
pub(crate) fn set_resource_version(value: &mut Value, mod_revision: u64) {
    let Some(obj) = value.as_object_mut() else {
        return;
    };
    let meta = obj
        .entry("metadata")
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    if let Some(m) = meta.as_object_mut() {
        m.insert(
            "resourceVersion".into(),
            Value::String(mod_revision.to_string()),
        );
    }
}

/// Read `metadata.name` from a JSON object.
pub(crate) fn object_name(value: &Value) -> Option<String> {
    value
        .get("metadata")?
        .get("name")?
        .as_str()
        .map(str::to_string)
}

/// Read `metadata.namespace` from a JSON object (if present).
pub(crate) fn object_namespace(value: &Value) -> Option<String> {
    value
        .get("metadata")?
        .get("namespace")?
        .as_str()
        .map(str::to_string)
}

/// Read `metadata.resourceVersion` (numeric string) as a [`u64`] revision.
pub(crate) fn resource_revision(value: &Value) -> Option<u64> {
    value
        .get("metadata")?
        .get("resourceVersion")?
        .as_str()?
        .parse::<u64>()
        .ok()
}

/// Build a per-item storage key from a resolved location, honouring scope.
pub(crate) fn item_key(loc: &Loc, res: &Resolved, name: &str) -> Key {
    match (res.scope.is_namespaced(), loc.namespace.as_deref()) {
        (true, Some(ns)) => namespaced_key(&loc.group, &loc.resource, ns, name),
        _ => cluster_key(&loc.group, &loc.resource, name),
    }
}

/// The error returned when a REST path names a resource that is not served
/// (upstream: `404 the server could not find the requested resource`).
pub(crate) const NOT_FOUND_RESOURCE_MSG: &str = "the server could not find the requested resource";
