//! Argo Workflows DAG + suspend template types (TODO **T7.3 Scope A**, decision **Q30**).
//!
//! 1:1 typed port of upstream `workflow_types.go` v4: [`DAGTemplate`],
//! [`DAGTask`], [`SuspendTemplate`], [`ContinueOn`], [`TemplateRef`] and
//! [`WorkflowTemplateRef`]. Field-level SSOT is the Go json tags; the wire
//! format is JSON-only (Q10).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::io::{Arguments, Sequence};
use super::is_false;
use super::lifecycle::LifecycleHooks;
use super::scalar::Item;
use super::template::Template;

/// DAGTemplate is a template subtype for directed acyclic graph templates.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DAGTemplate {
    /// Target are one or more names of targets to execute in a DAG.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub target: String,
    /// Tasks are a list of DAG tasks.
    #[serde(default)]
    pub tasks: Vec<DAGTask>,
    /// The DAG logic has a built-in "fail fast" feature to stop scheduling
    /// new steps as soon as one DAG node fails, then wait for all nodes to
    /// complete before failing the DAG itself. Defaults to true; when false
    /// every branch runs to completion regardless of failed outcomes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fail_fast: Option<bool>,
}

/// DAGTask represents a node in the graph during DAG execution.
///
/// Either `dependencies` or `depends` (not both) names the upstream tasks.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DAGTask {
    /// Name is the name of the target.
    #[serde(default)]
    pub name: String,
    /// Name of template to execute.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub template: String,
    /// Inline is the template. `template` must be empty if this is declared
    /// (and vice-versa). Free-form: preserved unknown fields on the wire.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inline: Option<Template>,
    /// Arguments are the parameter and artifact arguments to the template.
    #[serde(default)]
    pub arguments: Arguments,
    /// TemplateRef is the reference to the template resource to execute.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template_ref: Option<TemplateRef>,
    /// Dependencies are names of other targets which this depends on.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dependencies: Vec<String>,
    /// WithItems expands a task into multiple parallel tasks from the items
    /// in the list (free-form values).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub with_items: Vec<Item>,
    /// WithParam expands a task into multiple parallel tasks from the value
    /// in the parameter, which is expected to be a JSON list.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub with_param: String,
    /// WithSequence expands a task into a numeric sequence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub with_sequence: Option<Sequence>,
    /// When is an expression in which the task should conditionally execute.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub when: String,
    /// ContinueOn makes argo proceed with the following task even if this
    /// one fails; error and failed states can be specified.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continue_on: Option<ContinueOn>,
    /// OnExit is a template reference invoked at the end of the template,
    /// irrespective of success, failure, or error.
    ///
    /// Deprecated: use `hooks["exit"].template` instead.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub on_exit: String,
    /// Depends are names of other targets which this depends on.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub depends: String,
    /// Hooks hold the lifecycle hook invoked at the lifecycle of the task,
    /// irrespective of the success, failure, or error of the primary task.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub hooks: LifecycleHooks,
}

/// SuspendTemplate is a template subtype to suspend a workflow at a
/// predetermined point in time.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SuspendTemplate {
    /// Duration is the seconds to wait before automatically resuming a
    /// template. Must be a string; default unit is seconds, but may also be
    /// a Duration, e.g. "2m", "6h".
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub duration: String,
}

/// ContinueOn makes argo proceed with the following task even if the prior
/// task fails; error and failed states can be specified.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ContinueOn {
    /// Continue on node error.
    #[serde(default, skip_serializing_if = "is_false")]
    pub error: bool,
    /// Continue on node failure.
    #[serde(default, skip_serializing_if = "is_false")]
    pub failed: bool,
}

/// TemplateRef is a reference of template resource.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TemplateRef {
    /// Name is the resource name of the template.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    /// Template is the name of referred template in the resource.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub template: String,
    /// ClusterScope indicates the referred template is cluster scoped
    /// (i.e. a ClusterWorkflowTemplate).
    #[serde(default, skip_serializing_if = "is_false")]
    pub cluster_scope: bool,
}

/// WorkflowTemplateRef is a reference to a WorkflowTemplate resource.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowTemplateRef {
    /// Name is the resource name of the workflow template.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    /// ClusterScope indicates the referred template is cluster scoped
    /// (i.e. a ClusterWorkflowTemplate).
    #[serde(default, skip_serializing_if = "is_false")]
    pub cluster_scope: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dag_task_parses_and_reserializes_camel_case() {
        let task: DAGTask =
            serde_json::from_str(r#"{"name":"a","template":"t","depends":"b"}"#).unwrap();
        assert_eq!(task.name, "a");
        assert_eq!(task.template, "t");
        assert_eq!(task.depends, "b");
        assert!(task.continue_on.is_none());
        assert!(task.inline.is_none());

        let out = serde_json::to_value(&task).unwrap();
        assert_eq!(out["name"], "a");
        assert_eq!(out["template"], "t");
        assert_eq!(out["depends"], "b");
        // omitempty fields stay absent, not null.
        assert!(out.get("continueOn").is_none());
        assert!(out.get("withItems").is_none());
        assert!(out.get("templateRef").is_none());
    }

    #[test]
    fn suspend_template_duration_round_trip() {
        let suspend: SuspendTemplate = serde_json::from_str(r#"{"duration":"10s"}"#).unwrap();
        assert_eq!(suspend.duration, "10s");
        let out = serde_json::to_string(&suspend).unwrap();
        assert_eq!(out, r#"{"duration":"10s"}"#);

        // Zero value serializes to an empty object (Go omitempty parity).
        let empty = serde_json::to_string(&SuspendTemplate::default()).unwrap();
        assert_eq!(empty, "{}");
    }
}
