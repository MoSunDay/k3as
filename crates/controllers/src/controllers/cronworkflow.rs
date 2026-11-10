//! CronWorkflow reconciler adapter (TODO **T7.3 Scope C**, decision **Q30**;
//! extraction **Q42**).
//!
//! Scheduling (5-field cron next-fire math, active-child claim, concurrency
//! policy) lives in the pinned `arust-workflow` repo (`workflow-engine`,
//! Q38/Q42); status field spellings follow `api::argoproj::cron`. This
//! adapter bridges the in-process [`Client`] onto the engine's `Kube` seam.

use std::sync::Arc;

use serde_json::Value;

use crate::client::Client;
use crate::controllers::workflow_bridge::{from_engine, EngineClient};
use crate::error::ControllerError;

/// Reconcile one `argoproj.io/v1alpha1` CronWorkflow into scheduled child
/// Workflows.
pub async fn reconcile(client: &Arc<dyn Client>, cwf: &Value) -> Result<(), ControllerError> {
    workflow_engine::cron::reconcile(&EngineClient(client), cwf)
        .await
        .map_err(from_engine)
}
