//! Storage effects of the canary release (TODO **T8.5**, decision **Q58**):
//! the get/create-CAS upsert of the canary Deployment/Service, the
//! `TrafficSplit.spec.backends` patch and the abort-side deletes.
//!
//! Every write is get -> merge -> CAS (`resourceVersion`) so a lost race
//! surfaces as [`ControllerError::Conflict`] and the worker requeues
//! rate-limited instead of clobbering another writer; deletions tolerate
//! `NotFound` (the abort path must converge from any state).

use std::sync::Arc;

use serde_json::Value;
use storage::Key;

use crate::client::Client;
use crate::error::ControllerError;
use crate::object::{resource_version, semantic_eq};

use super::plan;

/// `TrafficSplit` API coordinates (T8.2/Q57).
const SPLIT_GROUP: &str = "init-pro.io";
const SPLIT_RESOURCE: &str = "trafficsplits";

/// Outcome of an [`ensure_deployment`] / [`ensure_service`] pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Ensured {
    /// Created or converged onto the desired shape.
    Ok,
    /// The object exists and is NOT controlled by this pipeline.
    Foreign,
}

/// `TrafficSplit` key inside the pipeline's namespace.
pub(super) fn split_key(ns: &str, name: &str) -> Key {
    Key::new(SPLIT_GROUP, SPLIT_RESOURCE, ns, name)
}

/// `client.get`, kept here so `mod.rs` reads as a sequence of steps.
pub(super) async fn get(
    client: &Arc<dyn Client>,
    key: &Key,
) -> Result<Option<Value>, ControllerError> {
    client.get(key).await
}

/// Create the canary Deployment or converge the owned one (never adopts).
pub(super) async fn ensure_deployment(
    client: &Arc<dyn Client>,
    key: &Key,
    desired: Value,
    pipeline: &str,
    uid: &str,
) -> Result<Ensured, ControllerError> {
    upsert(client, key, desired, plan::merge_deployment, pipeline, uid).await
}

/// Create the canary Service or converge the owned one (never adopts).
pub(super) async fn ensure_service(
    client: &Arc<dyn Client>,
    key: &Key,
    desired: Value,
    pipeline: &str,
    uid: &str,
) -> Result<Ensured, ControllerError> {
    upsert(client, key, desired, plan::merge_service, pipeline, uid).await
}

/// `TrafficSplit.spec.backends` <- `backends` (a missing split is fine).
pub(super) async fn patch_split(
    client: &Arc<dyn Client>,
    key: &Key,
    backends: Vec<api::traffic::SplitBackend>,
) -> Result<(), ControllerError> {
    let Some(current) = get(client, key).await? else {
        return Ok(()); // absent: the roll path reported it, abort tolerates it
    };
    let mut next = current.clone();
    next["spec"]["backends"] = serde_json::to_value(backends)
        .map_err(|e| ControllerError::Internal(format!("encode split backends: {e}")))?;
    if semantic_eq(&current, &next) {
        return Ok(()); // already on these weights
    }
    client
        .update(key, next, resource_version(&current))
        .await
        .map(|_| ())
        .map_err(|e| conflict(e, key))
}

/// Delete a canary child, tolerating its absence.
pub(super) async fn delete_if_present(
    client: &Arc<dyn Client>,
    key: &Key,
) -> Result<(), ControllerError> {
    match client.delete(key).await {
        Ok(()) => Ok(()),
        Err(e) if e.is_not_found() => Ok(()),
        Err(e) => Err(e),
    }
}

/// Shared upsert: create when absent, otherwise merge-or-noop with CAS.
async fn upsert(
    client: &Arc<dyn Client>,
    key: &Key,
    desired: Value,
    merge: fn(&Value, &Value) -> Option<Value>,
    pipeline: &str,
    uid: &str,
) -> Result<Ensured, ControllerError> {
    let current = match get(client, key).await? {
        Some(current) => Some(current),
        None => match client.create(key, desired.clone()).await {
            Ok(_) => return Ok(Ensured::Ok),
            Err(e) if e.is_already_exists() => get(client, key).await?,
            Err(e) => return Err(e),
        },
    };
    let Some(current) = current else {
        return Ok(Ensured::Ok); // deleted under us: the next pass decides
    };
    if !plan::owned_by(&current, pipeline, uid) {
        return Ok(Ensured::Foreign);
    }
    let Some(next) = merge(&current, &desired) else {
        return Ok(Ensured::Ok); // already converged
    };
    client
        .update(key, next, resource_version(&current))
        .await
        .map(|_| Ensured::Ok)
        .map_err(|e| conflict(e, key))
}

/// Map a storage CAS race onto the controller's retry signal.
fn conflict(error: ControllerError, key: &Key) -> ControllerError {
    match error.is_conflict() {
        true => ControllerError::Conflict(format!("canary write raced on {}", key.as_path())),
        false => error,
    }
}
