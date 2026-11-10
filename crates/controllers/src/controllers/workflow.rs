//! Argo Workflow reconciler adapter (TODO **T7.3 Scope B**, decision **Q30**;
//! extraction **Q42**).
//!
//! The reconciler itself — entrypoint compilation, node-tree status,
//! child Pod/Agent claims via the controller ownerReference +
//! `workflows.argoproj.io/node-name` annotation, group gating — lives in
//! the pinned `arust-workflow` repo (`workflow-engine`, Q38/Q42). This
//! adapter keeps the in-tree call shape (the runner hands the in-process
//! [`Client`]) and bridges it onto the engine's `Kube` seam via
//! [`crate::controllers::workflow_bridge::EngineClient`].

use std::sync::Arc;

use serde_json::Value;

use crate::client::Client;
use crate::controllers::workflow_bridge::{from_engine, EngineClient};
use crate::error::ControllerError;

/// Reconcile one `argoproj.io/v1alpha1` Workflow. `workspace_nfs` is the
/// anyhome endpoint for the step-pod `/workspace` auto-mount and the Agent
/// workspace contract annotation; `None` keeps children workspace-free.
pub async fn reconcile(
    client: &Arc<dyn Client>,
    wf: &Value,
    workspace_nfs: &Option<String>,
) -> Result<(), ControllerError> {
    workflow_engine::workflow::reconcile(&EngineClient(client), wf, workspace_nfs)
        .await
        .map_err(from_engine)
}
