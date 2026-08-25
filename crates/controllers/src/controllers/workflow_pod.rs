//! Workflow step-pod assembly (TODO **T7.3 Scope B**, decision **Q30**).
//!
//! Compiles one resolved step template + the workflow-level defaults into
//! a Pod object: deterministic name (from the plan), the authoritative
//! `workflows.argoproj.io/node-name` annotation, discovery labels, and a
//! controller ownerReference (GC cascade, Q20). Pod-spec semantics (v1
//! simplification, documented): `restartPolicy: Never`; the template's
//! `container` (name defaulted to "main") or its `script` container with
//! `command: ["sh","-c",<source>]` (image/env/resources kept verbatim);
//! workflow-level defaults overlaid template-over-spec for
//! serviceAccountName/nodeSelector/tolerations/affinity/imagePullSecrets/
//! schedulerName, `volumes` concatenated (template last), and
//! activeDeadlineSeconds from the template only. Only present fields are
//! emitted (the storage keeps the pod verbatim). JSON-only wire (Q10).

use serde_json::{json, Map, Value};

use crate::controllers::workflow_plan::PlannedStep;
use crate::controllers::workflow_status::NODE_NAME_ANNOTATION;
use crate::object::{name, owner_reference};

const API_VERSION: &str = "argoproj.io/v1alpha1";
const WORKFLOW_LABEL: &str = "workflows.argoproj.io/workflow";
const TEMPLATE_LABEL: &str = "workflows.argoproj.io/template";

/// Build one step pod (see the module doc for the spec semantics).
pub(crate) fn build_pod(wf: &Value, ns: &str, uid: &str, step: &PlannedStep) -> Value {
    let wf_name = name(wf).unwrap_or_default();
    let mut labels = Map::new();
    labels.insert(WORKFLOW_LABEL.to_string(), json!(wf_name));
    labels.insert(TEMPLATE_LABEL.to_string(), json!(step.template_name));
    let mut annotations = Map::new();
    annotations.insert(NODE_NAME_ANNOTATION.to_string(), json!(step.node_name));
    json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {
            "name": step.pod_name,
            "namespace": ns,
            "labels": labels,
            "annotations": annotations,
            "ownerReferences": [owner_reference(wf_name, uid, "Workflow", API_VERSION)],
        },
        "spec": build_pod_spec(wf.pointer("/spec").unwrap_or(&Value::Null), &step.template),
    })
}

/// Workflow defaults + template overlay -> pod spec fields.
pub(crate) fn build_pod_spec(wf_spec: &Value, template: &Value) -> Value {
    let mut spec = Map::new();
    spec.insert("restartPolicy".into(), json!("Never"));
    spec.insert("containers".into(), json!([main_container(template)]));
    for field in [
        "serviceAccountName",
        "nodeSelector",
        "tolerations",
        "affinity",
        "imagePullSecrets",
        "schedulerName",
    ] {
        if let Some(v) = overlay_field(template, wf_spec, field) {
            spec.insert(field.to_string(), v.clone());
        }
    }
    let volumes: Vec<Value> = [
        wf_spec.get("volumes").and_then(Value::as_array),
        template.get("volumes").and_then(Value::as_array),
    ]
    .into_iter()
    .flatten()
    .flatten()
    .cloned()
    .collect();
    if !volumes.is_empty() {
        spec.insert("volumes".into(), Value::Array(volumes));
    }
    if let Some(v) = template
        .get("activeDeadlineSeconds")
        .filter(|v| !v.is_null())
    {
        spec.insert("activeDeadlineSeconds".into(), v.clone());
    }
    Value::Object(spec)
}

/// Template field wins over the workflow-level default; empties omitted.
fn overlay_field<'a>(template: &'a Value, wf_spec: &'a Value, field: &str) -> Option<&'a Value> {
    template
        .get(field)
        .filter(|v| !is_empty_value(v))
        .or_else(|| wf_spec.get(field).filter(|v| !is_empty_value(v)))
}

fn is_empty_value(v: &Value) -> bool {
    match v {
        Value::Null => true,
        Value::String(s) => s.is_empty(),
        Value::Array(a) => a.is_empty(),
        Value::Object(m) => m.is_empty(),
        _ => false,
    }
}

/// The main container: `template.container` (name defaulted to "main"),
/// or the script template's container with `command: ["sh","-c",source]`.
fn main_container(template: &Value) -> Value {
    if let Some(script) = template.get("script").and_then(Value::as_object) {
        let mut c = script.get("container").cloned().unwrap_or(json!({}));
        if c.get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .is_empty()
        {
            c["name"] = json!("main");
        }
        // `script.image` is the template-level image; an explicit
        // `script.container.image` (if any) wins.
        if c.get("image")
            .and_then(Value::as_str)
            .unwrap_or("")
            .is_empty()
        {
            c["image"] = script.get("image").cloned().unwrap_or(Value::Null);
        }
        let source = script.get("source").and_then(Value::as_str).unwrap_or("");
        c["command"] = json!(["sh", "-c", source]);
        return c;
    }
    let mut c = template.get("container").cloned().unwrap_or(json!({}));
    if c.get("name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .is_empty()
    {
        c["name"] = json!("main");
    }
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn container_defaults_and_overlay() {
        let wf = json!({"spec": {
            "serviceAccountName": "wf-sa",
            "volumes": [{"name": "w"}],
            "schedulerName": "wf-sched",
        }});
        let template = json!({
            "container": {"image": "busybox", "command": ["true"]},
            "serviceAccountName": "step-sa",
            "volumes": [{"name": "t"}],
        });
        let pod = build_pod(
            &wf,
            "default",
            "uid-1",
            &PlannedStep {
                node_name: "wf[0].s".into(),
                template_name: "tmpl".into(),
                pod_name: "wf-0-s".into(),
                template: template.clone(),
            },
        );
        assert_eq!(pod["metadata"]["name"], "wf-0-s");
        assert_eq!(
            pod["metadata"]["annotations"]["workflows.argoproj.io/node-name"],
            "wf[0].s"
        );
        assert_eq!(
            pod["metadata"]["labels"]["workflows.argoproj.io/template"],
            "tmpl"
        );
        assert_eq!(pod["metadata"]["ownerReferences"][0]["kind"], "Workflow");
        assert_eq!(pod["spec"]["restartPolicy"], "Never");
        assert_eq!(pod["spec"]["containers"][0]["name"], "main");
        assert_eq!(pod["spec"]["containers"][0]["image"], "busybox");
        // Template wins; volumes concatenate (template last).
        assert_eq!(pod["spec"]["serviceAccountName"], "step-sa");
        assert_eq!(pod["spec"]["schedulerName"], "wf-sched");
        assert_eq!(
            pod["spec"]["volumes"]
                .as_array()
                .map(|v| v.iter().map(|x| x["name"].clone()).collect::<Vec<_>>()),
            Some(vec![json!("w"), json!("t")])
        );
        assert!(pod["spec"].get("affinity").is_none());
    }

    #[test]
    fn script_template_runs_via_sh() {
        let spec = build_pod_spec(
            &json!({}),
            &json!({
                "script": {"image": "python", "source": "print(1)", "container": {"env": [{"name": "E", "value": "1"}]}},
            }),
        );
        let c = &spec["containers"][0];
        assert_eq!(c["name"], "main");
        assert_eq!(c["image"], "python");
        assert_eq!(c["command"], json!(["sh", "-c", "print(1)"]));
        assert_eq!(c["env"][0]["name"], "E");
    }
}
