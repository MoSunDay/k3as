//! Workflow Agent-child assembly (TODO **A5**, decisions **Q30** + **Q35**).
//!
//! A Workflow template with an `agent` block no longer materializes a Pod:
//! the controller creates an `init-pro.io/v1` **Agent** child object whose
//! spec IS the template's `agent` object (camelCase passthrough:
//! `nodeName`/`nodeSelector`/`tolerations`/`affinity`/`schedulerName`/
//! `resources`/`agent`/`prompt`/`model`/`provider`). The metadata follows
//! [`workflow_pod::build_pod`] exactly -- deterministic child name (the
//! plan's [`pod_name_for`](super::workflow_plan) projection), the
//! discovery labels, the authoritative `workflows.argoproj.io/node-name`
//! annotation and a controller ownerReference back to the Workflow (GC
//! cascade + phase fan-out, Q20). `status` is NOT set at creation: a fresh
//! Agent decodes as `Pending` server-side, and the scheduler (binding) and
//! agent runtime (execution) own every later phase transition.
//!
//! JSON-only wire (Q10); pure functions, no classes.

use serde_json::{json, Value};

use crate::controllers::workflow_plan::PlannedStep;
use crate::controllers::workflow_pod::{node_annotation, step_labels};
use crate::object::{name, owner_reference};

/// API group/version of the child object (the `init-pro.io` CRD group).
const AGENT_API_VERSION: &str = "init-pro.io/v1";
/// API group/version of the owning Workflow (the ownerReference must name
/// the group the apiserver registered the Workflow under).
const WORKFLOW_API_VERSION: &str = "argoproj.io/v1alpha1";

/// Build one step's Agent child object. `spec` is the template's `agent`
/// object verbatim (a non-object/missing `agent` yields an empty spec --
/// the plan only routes `agent` templates here, so this is defensive).
pub(crate) fn build_agent(wf: &Value, ns: &str, uid: &str, step: &PlannedStep) -> Value {
    let wf_name = name(wf).unwrap_or_default();
    let spec = step
        .template
        .get("agent")
        .filter(|v| v.as_object().is_some())
        .cloned()
        .unwrap_or_else(|| json!({}));
    json!({
        "apiVersion": AGENT_API_VERSION,
        "kind": "Agent",
        "metadata": {
            "name": step.pod_name,
            "namespace": ns,
            "labels": step_labels(wf_name, &step.template_name),
            "annotations": node_annotation(&step.node_name),
            "ownerReferences": [
                owner_reference(wf_name, uid, "Workflow", WORKFLOW_API_VERSION)
            ],
        },
        "spec": spec,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controllers::workflow_plan::StepTarget;
    use serde_json::json;

    fn step(agent: Value) -> PlannedStep {
        PlannedStep {
            node_name: "wf[0].ask".into(),
            template_name: "tmpl".into(),
            pod_name: "wf-0-ask".into(),
            template: agent,
            target: StepTarget::Agent,
        }
    }

    #[test]
    fn agent_child_carries_owner_annotation_and_spec_passthrough() {
        let wf = json!({
            "metadata": {"name": "wf", "namespace": "default", "uid": "u-1"},
            "spec": {"entrypoint": "main"},
        });
        let template = json!({
            "agent": {
                "nodeName": "gpu-1",
                "nodeSelector": {"pool": "gpu"},
                "prompt": "summarize",
                "model": {"name": "small"},
                "provider": "local",
            },
        });
        let agent = build_agent(&wf, "default", "u-1", &step(template));
        assert_eq!(agent["apiVersion"], "init-pro.io/v1");
        assert_eq!(agent["kind"], "Agent");
        assert_eq!(agent["metadata"]["name"], "wf-0-ask");
        assert_eq!(agent["metadata"]["namespace"], "default");
        // Same discovery labels as a step pod (workflow_pod parity).
        assert_eq!(
            agent["metadata"]["labels"]["workflows.argoproj.io/workflow"],
            "wf"
        );
        assert_eq!(
            agent["metadata"]["labels"]["workflows.argoproj.io/template"],
            "tmpl"
        );
        // The node-name annotation is the phase-lookup key.
        assert_eq!(
            agent["metadata"]["annotations"]["workflows.argoproj.io/node-name"],
            "wf[0].ask"
        );
        // Controller ownerReference back to the Workflow (GC cascade, Q20).
        let owner = &agent["metadata"]["ownerReferences"][0];
        assert_eq!(owner["kind"], "Workflow");
        assert_eq!(owner["apiVersion"], "argoproj.io/v1alpha1");
        assert_eq!(owner["name"], "wf");
        assert_eq!(owner["uid"], "u-1");
        assert_eq!(owner["controller"], true);
        assert_eq!(owner["blockOwnerDeletion"], true);
        // Verbatim camelCase spec passthrough.
        assert_eq!(agent["spec"]["nodeName"], "gpu-1");
        assert_eq!(agent["spec"]["nodeSelector"]["pool"], "gpu");
        assert_eq!(agent["spec"]["prompt"], "summarize");
        assert_eq!(agent["spec"]["model"]["name"], "small");
        assert_eq!(agent["spec"]["provider"], "local");
        // Status is NOT set at creation (Pending is the server-side default).
        assert!(agent.get("status").is_none());
    }

    #[test]
    fn non_object_agent_block_defaults_to_empty_spec() {
        let wf = json!({"metadata": {"name": "wf"}});
        let agent = build_agent(&wf, "default", "", &step(json!({"agent": "oops"})));
        assert_eq!(agent["spec"], json!({}));
        let bare = build_agent(&wf, "default", "", &step(json!({})));
        assert_eq!(bare["spec"], json!({}));
    }
}
