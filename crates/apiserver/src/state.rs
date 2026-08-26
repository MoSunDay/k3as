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

/// Read `metadata.generateName` (the POST-create name prefix, upstream parity).
pub(crate) fn object_generate_name(value: &Value) -> Option<&str> {
    value["metadata"]["generateName"]
        .as_str()
        .filter(|n| !n.is_empty())
}

/// Set `metadata.name` on an object (generateName suffixing, before store).
pub(crate) fn set_object_name(value: &mut Value, name: &str) {
    if let Some(obj) = value.as_object_mut() {
        let meta = obj
            .entry("metadata")
            .or_insert_with(|| Value::Object(serde_json::Map::new()));
        if let Some(m) = meta.as_object_mut() {
            m.insert("name".into(), Value::String(name.to_string()));
        }
    }
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

/// Upstream parity (T4.2 Scope C): every object creation is assigned a
/// `metadata.uid` (k8s generates a UUID on POST / SSA-create). Fills only
/// when absent-or-empty; callers pass the freshly built object before it
/// hits the store. The v1 relay (pod log/exec) and the kubelet log
/// directory layout both key off this uid, so a missing one used to strand
/// the whole data plane.
pub(crate) fn ensure_object_uid(v: &mut Value) {
    let blank = match v.get("metadata").and_then(|m| m.get("uid")) {
        None | Some(Value::Null) => true,
        Some(x) => x.as_str().map(str::is_empty).unwrap_or(false),
    };
    if blank
        && v.get("metadata")
            .map(serde_json::Value::is_object)
            .unwrap_or(false)
    {
        if let Some(m) = v.get_mut("metadata") {
            m["uid"] = Value::String(new_uid());
        }
    }
}

/// Fresh Kubernetes-shaped v4 UUID via the already-pinned `getrandom`
/// (same entropy source as collection.rs `random_suffix`).
fn new_uid() -> String {
    use std::fmt::Write as _;
    let mut b = [0u8; 16];
    getrandom::fill(&mut b).expect("system randomness unavailable");
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let mut out = String::with_capacity(36);
    for (i, x) in b.iter().enumerate() {
        if matches!(i, 4 | 6 | 8 | 10) {
            out.push('-');
        }
        let _ = write!(out, "{x:02x}");
    }
    out
}

#[cfg(test)]
mod uid_tests {
    use super::*;

    #[test]
    fn ensure_object_uid_fills_missing_and_keeps_explicit() {
        let mut v: Value = serde_json::from_str(r#"{"metadata":{"name":"p"},"spec":{}}"#).unwrap();
        ensure_object_uid(&mut v);
        let uid = v.pointer("/metadata/uid").and_then(Value::as_str).unwrap();
        assert_eq!(uid.len(), 36);
        assert_eq!(
            uid.matches('-').count(),
            4,
            "Kubernetes v4 UUID shape expected"
        );

        // Explicit non-empty uid survives untouched.
        let mut kept: Value =
            serde_json::from_str(r#"{"metadata":{"name":"p","uid":"explicit-uid"}}"#).unwrap();
        ensure_object_uid(&mut kept);
        assert_eq!(
            kept.pointer("/metadata/uid").cloned(),
            Some(Value::String("explicit-uid".into()))
        );

        // Null and empty-string uids count as blank (regenerated).
        for raw in [r#"{"metadata":{"uid":null}}"#, r#"{"metadata":{"uid":""}}"#] {
            let mut b: Value = serde_json::from_str(raw).unwrap();
            ensure_object_uid(&mut b);
            assert_eq!(
                b.pointer("/metadata/uid")
                    .and_then(Value::as_str)
                    .unwrap()
                    .len(),
                36
            );
        }

        // No metadata object: leave the body alone (creation validates it).
        let mut none: Value = serde_json::json!({"spec": {}});
        ensure_object_uid(&mut none);
        assert!(none.get("metadata").is_none());
    }
}
