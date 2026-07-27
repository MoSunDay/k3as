//! `/registry/...` key layout -- upstream kube-apiserver etcd parity (T2.2).
//!
//! Layout:
//!  - core group (`""`), namespaced: `/registry/<resource>/<namespace>/<name>`
//!  - core group, cluster-scoped:  `/registry/<resource>/<name>`
//!  - non-core group, namespaced:  `/registry/<group>/<resource>/<namespace>/<name>`
//!  - non-core group, cluster:     `/registry/<group>/<resource>/<name>`
//!
//! This matches `etcdctl get /registry/pods --prefix`.

use crate::error::StorageError;

const REGISTRY: &str = "/registry";

/// A fully-qualified storage key for one resource object.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Key {
    /// API group; empty for the core (`""`) group.
    pub group: String,
    /// Plural resource name, e.g. `"pods"`.
    pub resource: String,
    /// Namespace; empty for cluster-scoped resources.
    pub namespace: String,
    /// Object name.
    pub name: String,
}

impl Key {
    pub fn new(
        group: impl Into<String>,
        resource: impl Into<String>,
        namespace: impl Into<String>,
        name: impl Into<String>,
    ) -> Self {
        Self {
            group: group.into(),
            resource: resource.into(),
            namespace: namespace.into(),
            name: name.into(),
        }
    }

    /// Render the canonical `/registry/...` path.
    pub fn as_path(&self) -> String {
        let g_empty = self.group.is_empty();
        let ns_empty = self.namespace.is_empty();
        match (g_empty, ns_empty) {
            (true, true) => format!("{REGISTRY}/{r}/{name}", r = self.resource, name = self.name),
            (true, false) => format!(
                "{REGISTRY}/{r}/{ns}/{name}",
                r = self.resource,
                ns = self.namespace,
                name = self.name
            ),
            (false, true) => format!(
                "{REGISTRY}/{g}/{r}/{name}",
                g = self.group,
                r = self.resource,
                name = self.name
            ),
            (false, false) => format!(
                "{REGISTRY}/{g}/{r}/{ns}/{name}",
                g = self.group,
                r = self.resource,
                ns = self.namespace,
                name = self.name
            ),
        }
    }
}

/// A collection prefix for `list`/`watch`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct KeyPrefix {
    pub group: String,
    pub resource: String,
    /// `None` = all namespaces.
    pub namespace: Option<String>,
}

impl KeyPrefix {
    pub fn new(
        group: impl Into<String>,
        resource: impl Into<String>,
        namespace: Option<String>,
    ) -> Self {
        Self {
            group: group.into(),
            resource: resource.into(),
            namespace,
        }
    }

    /// Render the canonical `/registry/...` collection path (no trailing `/`;
    /// the consumer appends `/` when ranging to delimit path segments).
    pub fn as_path(&self) -> String {
        let g_empty = self.group.is_empty();
        match (g_empty, &self.namespace) {
            (true, None) => format!("{REGISTRY}/{r}", r = self.resource),
            (true, Some(ns)) => format!("{REGISTRY}/{r}/{ns}", r = self.resource),
            (false, None) => format!("{REGISTRY}/{g}/{r}", g = self.group, r = self.resource),
            (false, Some(ns)) => {
                format!("{REGISTRY}/{g}/{r}/{ns}", g = self.group, r = self.resource)
            }
        }
    }
}

/// Validate that a key's segments are non-empty where required.
pub(crate) fn validate(k: &Key) -> Result<(), StorageError> {
    if k.resource.is_empty() || k.name.is_empty() {
        return Err(StorageError::InvalidKey { key: k.as_path() });
    }
    Ok(())
}
