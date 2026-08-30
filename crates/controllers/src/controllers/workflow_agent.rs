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
//!
//! T-anyhome (decision **Q36**): with an anyhome endpoint configured,
//! [`build_agent_with`] stamps the `init-pro.io/workspace` contract
//! annotation (the NFS URL backing this Agent's memory workspace). Phase B
//! (agent execution) and kubectl consume this annotation; the mount target
//! is [`common::workspace::MOUNT_TARGET`] (`/workspace`).

use serde_json::{json, Value};

use crate::controllers::workflow_plan::PlannedStep;
use crate::controllers::workflow_pod::{node_annotation, step_labels};
use crate::object::{name, owner_reference};
use common::workspace;

/// API group/version of the child object (the `init-pro.io` CRD group).
const AGENT_API_VERSION: &str = "init-pro.io/v1";
/// API group/version of the owning Workflow (the ownerReference must name
/// the group the apiserver registered the Workflow under).
const WORKFLOW_API_VERSION: &str = "argoproj.io/v1alpha1";

/// The workspace contract annotation (T-anyhome, Q36): the anyhome NFS URL
/// (`nfs://<addr>/agent/<child-name>`) backing the Agent's memory
/// workspace, mounted at `common::workspace::MOUNT_TARGET`.
pub const WORKSPACE_ANNOTATION: &str = "init-pro.io/workspace";

/// Build one step's Agent child object (pre-Q36 shape: no workspace
/// contract annotation -- [`build_agent_with`] with `None`).
pub fn build_agent(wf: &Value, ns: &str, uid: &str, step: &PlannedStep) -> Value {
    build_agent_with(wf, ns, uid, step, None)
}

/// Build one step's Agent child object, extending the annotation map with
/// the workspace contract (T-anyhome, Q36) when `workspace_nfs` is set and
/// `step.pod_name` (the child Agent's name) passes the SSOT path guard;
/// invalid/None -> no annotation. `spec` is the template's `agent` object
/// verbatim (a non-object/missing `agent` yields an empty spec -- the plan
/// only routes `agent` templates here, so this is defensive).
pub fn build_agent_with(
    wf: &Value,
    ns: &str,
    uid: &str,
    step: &PlannedStep,
    workspace_nfs: Option<&str>,
) -> Value {
    let wf_name = name(wf).unwrap_or_default();
    let spec = step
        .template
        .get("agent")
        .filter(|v| v.as_object().is_some())
        .cloned()
        .unwrap_or_else(|| json!({}));
    // T-anyhome (Q36): extend the node-name annotation map, never replace.
    let mut annotations = node_annotation(&step.node_name);
    if let Some(addr) = workspace_nfs {
        if let Some(path) = workspace::agent_workspace_path(&step.pod_name) {
            annotations.insert(
                WORKSPACE_ANNOTATION.to_string(),
                json!(workspace::workspace_nfs_url(addr, &path)),
            );
        }
    }
    json!({
        "apiVersion": AGENT_API_VERSION,
        "kind": "Agent",
        "metadata": {
            "name": step.pod_name,
            "namespace": ns,
            "labels": step_labels(wf_name, &step.template_name),
            "annotations": annotations,
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

    // ---- workspace contract annotation (T-anyhome, Q36) ----

    #[test]
    fn workspace_annotation_carries_the_nfs_contract_url() {
        let wf = json!({"metadata": {"name": "wf"}});
        let agent = build_agent_with(
            &wf,
            "default",
            "u-1",
            &step(json!({"agent": {}})),
            Some("anyhome.default.svc:2049"),
        );
        // Exact contract: nfs://<addr>/agent/<child-name>; the child name IS
        // step.pod_name.
        assert_eq!(
            agent["metadata"]["annotations"][WORKSPACE_ANNOTATION],
            "nfs://anyhome.default.svc:2049/agent/wf-0-ask"
        );
        // The node-name annotation map was EXTENDED, not replaced.
        assert_eq!(
            agent["metadata"]["annotations"]["workflows.argoproj.io/node-name"],
            "wf[0].ask"
        );
    }

    #[test]
    fn workspace_annotation_absent_without_endpoint() {
        let wf = json!({"metadata": {"name": "wf"}});
        let agent = build_agent_with(&wf, "default", "u-1", &step(json!({"agent": {}})), None);
        assert!(
            agent["metadata"]["annotations"]
                .get(WORKSPACE_ANNOTATION)
                .is_none(),
            "no anyhome endpoint -> no workspace contract"
        );
        // The pre-Q36 builder stays annotation-identical.
        let plain = build_agent(&wf, "default", "u-1", &step(json!({"agent": {}})));
        assert_eq!(
            agent["metadata"]["annotations"],
            plain["metadata"]["annotations"]
        );
    }

    #[test]
    fn workspace_annotation_absent_for_invalid_child_id() {
        let wf = json!({"metadata": {"name": "wf"}});
        let mut bad = step(json!({"agent": {}}));
        bad.pod_name = String::new(); // fails the SSOT path guard
        let agent = build_agent_with(&wf, "default", "u-1", &bad, Some("10.0.0.5:2049"));
        assert!(
            agent["metadata"]["annotations"]
                .get(WORKSPACE_ANNOTATION)
                .is_none(),
            "invalid child id -> no annotation, no panic"
        );
        assert_eq!(
            agent["metadata"]["annotations"]["workflows.argoproj.io/node-name"], "wf[0].ask",
            "node-name annotation is kept regardless"
        );
    }
}
