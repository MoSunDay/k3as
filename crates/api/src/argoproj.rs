//! `argoproj.io/v1alpha1` API group wiring (TODO **T7.3**, decision **Q30**).
//!
//! The 1:1 typed port of the upstream Argo Workflows v4 resource model now
//! lives in the pinned `arust-workflow` repo as `workflow-model` (extraction
//! decision **Q42**, link_repos pin-clone Q38); everything is re-exported
//! here so `api::argoproj::*` consumer paths are unchanged. This shim keeps
//! only the embedder-side concern: registering the kinds into THIS crate's
//! [`SchemaRegistry`], which is what turns the generic apiserver routes into
//! full CRUD + watch + discovery for the group. Wire format is JSON-only
//! (decision **Q10**).

pub use workflow_model::*;

use crate::schema::{SchemaRegistry, Scope};

/// Register the `argoproj.io/v1alpha1` kinds into a [`SchemaRegistry`]:
/// `workflows` (short name `wf`) and `cronworkflows` (`cwf`, `cronwf`),
/// both namespaced (Q30). Registration is what turns the generic apiserver
/// routes into full CRUD + watch + discovery for the group.
pub fn register(reg: &mut SchemaRegistry) {
    reg.register_with_short_names(
        GROUP,
        VERSION,
        "Workflow",
        "workflows",
        "WorkflowList",
        Scope::Namespaced,
        &["wf"],
    );
    reg.register_with_short_names(
        GROUP,
        VERSION,
        "CronWorkflow",
        "cronworkflows",
        "CronWorkflowList",
        Scope::Namespaced,
        &["cwf", "cronwf"],
    );
}
