//! Storage-backed [`Client`] (T3.1a, decision **Q19**).
//!
//! v1 controllers run in the same process as the apiserver and share its
//! `Arc<dyn StorageBackend>`; this trait is the seam where an HTTP-backed
//! client (T3.4, HA / out-of-process controllers) slots in later. Every
//! returned object carries `metadata.resourceVersion` (projected from the
//! store's `mod_revision`, the k8s resourceVersion) and
//! `metadata.namespace` for namespaced keys, mirroring the apiserver's
//! `state.rs` wire projection.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use storage::{Key, KeyPrefix, StorageBackend, StoredEntry, Watch};

use crate::error::ControllerError;

/// Read/write/watch surface consumed by informers and reconcilers.
#[async_trait]
pub trait Client: Send + Sync {
    async fn list(&self, prefix: &KeyPrefix) -> Result<Vec<Value>, ControllerError>;
    async fn get(&self, key: &Key) -> Result<Option<Value>, ControllerError>;
    async fn create(&self, key: &Key, value: Value) -> Result<Value, ControllerError>;
    async fn update(
        &self,
        key: &Key,
        value: Value,
        if_revision: Option<u64>,
    ) -> Result<Value, ControllerError>;
    async fn delete(&self, key: &Key) -> Result<(), ControllerError>;
    async fn watch(
        &self,
        prefix: &KeyPrefix,
        start_revision: Option<u64>,
    ) -> Result<Watch, ControllerError>;
}

/// In-process client over a shared storage backend (Q19).
#[derive(Clone)]
pub struct StorageClient {
    store: Arc<dyn StorageBackend>,
}

impl StorageClient {
    pub fn new(store: Arc<dyn StorageBackend>) -> Self {
        Self { store }
    }

    /// Project a stored entry onto the k8s wire shape: resourceVersion from
    /// `mod_revision`, namespace from the key path.
    fn project(entry: StoredEntry, namespace: Option<String>) -> Value {
        let mut v = entry.value;
        set_resource_version(&mut v, entry.mod_revision);
        if let Some(ns) = namespace {
            set_namespace(&mut v, &ns);
        }
        v
    }
}

/// Recover the namespace segment of a `/registry/[g/]r/[ns/]name` path.
/// `group` disambiguates the 4-segment cluster-vs-namespaced case.
fn namespace_of_path(path: &str, group: &str) -> Option<String> {
    let mut segs: Vec<&str> = path
        .strip_prefix("/registry/")
        .unwrap_or(path)
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();
    if !group.is_empty() {
        if segs.is_empty() {
            return None;
        }
        segs.remove(0);
    }
    match segs.len() {
        3 => Some(segs[1].to_string()),
        _ => None,
    }
}

/// Set `metadata.resourceVersion` (local copy of the apiserver helper).
fn set_resource_version(value: &mut Value, mod_revision: u64) {
    if let Some(obj) = value.as_object_mut() {
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
}

/// Set `metadata.namespace` (local copy of the apiserver helper).
fn set_namespace(value: &mut Value, namespace: &str) {
    if let Some(obj) = value.as_object_mut() {
        let meta = obj
            .entry("metadata")
            .or_insert_with(|| Value::Object(serde_json::Map::new()));
        if let Some(m) = meta.as_object_mut() {
            m.insert("namespace".into(), Value::String(namespace.to_string()));
        }
    }
}

#[async_trait]
impl Client for StorageClient {
    async fn list(&self, prefix: &KeyPrefix) -> Result<Vec<Value>, ControllerError> {
        let entries = self.store.list(prefix).await?;
        Ok(entries
            .into_iter()
            .map(|e| {
                let ns = namespace_of_path(&e.key, &prefix.group);
                StorageClient::project(e, ns)
            })
            .collect())
    }

    async fn get(&self, key: &Key) -> Result<Option<Value>, ControllerError> {
        let ns = (!key.namespace.is_empty()).then(|| key.namespace.clone());
        Ok(self
            .store
            .get(key)
            .await?
            .map(|e| StorageClient::project(e, ns)))
    }

    async fn create(&self, key: &Key, value: Value) -> Result<Value, ControllerError> {
        let entry = self.store.create(key, value).await?;
        let ns = (!key.namespace.is_empty()).then(|| key.namespace.clone());
        Ok(StorageClient::project(entry, ns))
    }

    async fn update(
        &self,
        key: &Key,
        value: Value,
        if_revision: Option<u64>,
    ) -> Result<Value, ControllerError> {
        let entry = self.store.update(key, value, if_revision).await?;
        let ns = (!key.namespace.is_empty()).then(|| key.namespace.clone());
        Ok(StorageClient::project(entry, ns))
    }

    async fn delete(&self, key: &Key) -> Result<(), ControllerError> {
        self.store.delete(key, None).await?;
        Ok(())
    }

    async fn watch(
        &self,
        prefix: &KeyPrefix,
        start_revision: Option<u64>,
    ) -> Result<Watch, ControllerError> {
        Ok(self.store.watch(prefix, start_revision).await?)
    }
}
