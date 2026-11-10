//! Bridge from the embedded `arust-workflow` engine to the in-process
//! controller client (extraction decision **Q42**; heritage T7.3, Q30).
//!
//! [`EngineClient`] implements the engine's [`workflow_engine::api::Kube`]
//! seam over this crate's [`Client`] (Q19): keys map 1:1 onto
//! `storage::{Key, KeyPrefix}` and errors map losslessly in both directions
//! (CAS races stay `Conflict`, AlreadyExists/NotFound keep their
//! controller-visible shapes so the reconcilers' benign-error guards keep
//! their exact semantics).

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use workflow_engine::api::{EngineError, Key, KeyPrefix, Kube};

use crate::client::Client;
use crate::error::ControllerError;
use storage::{Key as StoreKey, KeyPrefix as StoreKeyPrefix, StorageError};

/// `Kube` implementation over the shared in-process client (Q19).
pub(crate) struct EngineClient<'a>(pub &'a Arc<dyn Client>);

fn store_key(k: &Key) -> StoreKey {
    StoreKey::new(
        k.group.clone(),
        k.resource.clone(),
        k.namespace.clone().unwrap_or_default(),
        k.name.clone(),
    )
}

fn store_prefix(p: &KeyPrefix) -> StoreKeyPrefix {
    StoreKeyPrefix::new(p.group.clone(), p.resource.clone(), p.namespace.clone())
}

/// Embedder -> engine: keep every benign shape the reconcilers guard on.
fn to_engine(e: ControllerError) -> EngineError {
    match e {
        ControllerError::Conflict(m) => EngineError::Conflict(m),
        ControllerError::Internal(m) => EngineError::Internal(m),
        ControllerError::Storage(StorageError::AlreadyExists { key }) => {
            EngineError::AlreadyExists(key)
        }
        ControllerError::Storage(StorageError::NotFound { key }) => EngineError::NotFound(key),
        ControllerError::Storage(StorageError::Conflict { .. }) => {
            EngineError::Conflict(e.to_string())
        }
        other => EngineError::Storage(other.to_string()),
    }
}

/// Engine -> embedder: the runner's `ControllerError` surface is unchanged.
pub(crate) fn from_engine(e: EngineError) -> ControllerError {
    match e {
        EngineError::Conflict(m) => ControllerError::Conflict(m),
        EngineError::Internal(m) => ControllerError::Internal(m),
        EngineError::AlreadyExists(key) => {
            ControllerError::Storage(StorageError::AlreadyExists { key })
        }
        EngineError::NotFound(key) => ControllerError::Storage(StorageError::NotFound { key }),
        EngineError::Storage(s) => ControllerError::Internal(s),
    }
}

#[async_trait]
impl Kube for EngineClient<'_> {
    async fn list(&self, prefix: &KeyPrefix) -> Result<Vec<Value>, EngineError> {
        self.0.list(&store_prefix(prefix)).await.map_err(to_engine)
    }
    async fn get(&self, key: &Key) -> Result<Option<Value>, EngineError> {
        self.0.get(&store_key(key)).await.map_err(to_engine)
    }
    async fn create(&self, key: &Key, value: Value) -> Result<Value, EngineError> {
        self.0
            .create(&store_key(key), value)
            .await
            .map_err(to_engine)
    }
    async fn update(
        &self,
        key: &Key,
        value: Value,
        if_revision: Option<u64>,
    ) -> Result<Value, EngineError> {
        self.0
            .update(&store_key(key), value, if_revision)
            .await
            .map_err(to_engine)
    }
    async fn delete(&self, key: &Key) -> Result<(), EngineError> {
        self.0.delete(&store_key(key)).await.map_err(to_engine)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_contract_mirror_matches_common_ssot() {
        // Q36: `common::workspace` is the binary-side SSOT; the engine's
        // mirror must not drift (Q42).
        assert_eq!(
            workflow_engine::workspace::MOUNT_TARGET,
            common::workspace::MOUNT_TARGET
        );
        assert_eq!(
            workflow_engine::workspace::WORKSPACE_ANNOTATION,
            common::workspace::WORKSPACE_ANNOTATION
        );
        assert_eq!(
            workflow_engine::workspace::workflow_workspace_path("wf-1"),
            common::workspace::workflow_workspace_path("wf-1")
        );
        assert_eq!(
            workflow_engine::workspace::agent_workspace_path("ag-1"),
            common::workspace::agent_workspace_path("ag-1")
        );
    }

    #[test]
    fn rfc3339_contract_mirror_matches_common_ssot() {
        for t in [0u64, 951_782_400, 1_700_000_015] {
            assert_eq!(
                workflow_engine::time::now_rfc3339(t),
                common::time::now_rfc3339(t)
            );
            let s = workflow_engine::time::now_rfc3339(t);
            assert_eq!(
                workflow_engine::time::parse_rfc3339(&s),
                common::time::parse_rfc3339(&s)
            );
        }
    }

    #[test]
    fn error_mapping_roundtrips_benign_shapes() {
        let ce = ControllerError::Storage(StorageError::AlreadyExists { key: "/k".into() });
        let ee = to_engine(ce);
        assert!(ee.is_already_exists());
        assert!(matches!(
            from_engine(ee),
            ControllerError::Storage(StorageError::AlreadyExists { .. })
        ));
        let nf = to_engine(ControllerError::Storage(StorageError::NotFound {
            key: "/k".into(),
        }));
        assert!(nf.is_not_found());
    }
}
