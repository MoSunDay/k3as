//! `argoproj.io/v1alpha1` API group types (TODO **T7.3 Scope A**, Q30).
//!
//! 1:1 typed port of the upstream Argo Workflows v4 resource model — the
//! field-level SSOT is the Go json tags in
//! `link_repos/argo-workflows/pkg/apis/workflow/v1alpha1`. Scope A ships the
//! two first-class kinds the platform serves and stores:
//! [`Workflow`] and [`CronWorkflow`] (schema-only — registration is CRUD via
//! the generic apiserver routes; a workflow controller is a later scope).
//! Wire format is JSON-only (decision **Q10**); embedded k8s core types are
//! reused from `k8s_openapi`, not redefined.

pub mod artifact;
pub mod container_set;
pub mod cron;
pub mod dag;
pub mod http;
pub mod io;
pub mod lifecycle;
pub mod metrics;
pub mod phases;
pub mod repo;
pub mod scalar;
pub mod status;
pub mod stores;
pub mod sync;
pub mod template;
pub mod workflow;

use crate::schema::{SchemaRegistry, Scope};

/// API group for the Argo Workflows resources.
pub const GROUP: &str = "argoproj.io";
/// Served version (upstream GA group version for Workflow/CronWorkflow).
pub const VERSION: &str = "v1alpha1";
/// `group/version` wire string.
pub const API_VERSION: &str = "argoproj.io/v1alpha1";

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

// Convenience re-exports (root kinds + the types clients build most).
pub use cron::{CronWorkflow, CronWorkflowList, CronWorkflowSpec, CronWorkflowStatus};
pub use status::WorkflowStatus;
pub use template::Template;
pub use workflow::{Workflow, WorkflowList, WorkflowSpec};

// --- serde skip-helpers shared by the modules above (Go omitempty parity) ---

/// `bool` zero-check for `#[serde(skip_serializing_if)]` (Go omitempty).
pub(crate) fn is_false(b: &bool) -> bool {
    !*b
}

/// `i32` zero-check (Go omitempty on ints).
pub(crate) fn is_zero_i32(v: &i32) -> bool {
    *v == 0
}

/// `i64` zero-check (Go omitempty on longs).
pub(crate) fn is_zero_i64(v: &i64) -> bool {
    *v == 0
}
