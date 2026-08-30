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
//!
//! T-anyhome (decision **Q36**): [`build_pod_with`] additionally appends
//! the Workflow's SHARED `/workspace` NFS volume + volumeMount from the
//! anyhome endpoint -- unless the user already wired a workspace themselves
//! (user declaration always wins). `build_pod` keeps the pre-Q36,
//! workspace-free builder shape.

use serde_json::{json, Map, Value};

use crate::controllers::workflow_plan::PlannedStep;
use crate::controllers::workflow_status::NODE_NAME_ANNOTATION;
use crate::object::{name, owner_reference};
use common::workspace;

const API_VERSION: &str = "argoproj.io/v1alpha1";
const WORKFLOW_LABEL: &str = "workflows.argoproj.io/workflow";
const TEMPLATE_LABEL: &str = "workflows.argoproj.io/template";

/// Discovery labels shared by every step child object (A5): the owning
/// workflow and the resolved template name (upstream parity with the pod
/// labels `workflows.argoproj.io/workflow` / `/template`).
pub(crate) fn step_labels(wf_name: &str, template_name: &str) -> Map<String, Value> {
    let mut labels = Map::new();
    labels.insert(WORKFLOW_LABEL.to_string(), json!(wf_name));
    labels.insert(TEMPLATE_LABEL.to_string(), json!(template_name));
    labels
}

/// The authoritative child -> node mapping annotation; stamped on step
/// pods AND Agent children (A5/Q35) so one lookup serves both targets.
pub(crate) fn node_annotation(node_name: &str) -> Map<String, Value> {
    let mut annotations = Map::new();
    annotations.insert(NODE_NAME_ANNOTATION.to_string(), json!(node_name));
    annotations
}

/// Build one step pod (see the module doc for the spec semantics). The
/// pre-Q36 shape: no `/workspace` auto-mount (`build_pod_with(.., None)`).
pub fn build_pod(wf: &Value, ns: &str, uid: &str, step: &PlannedStep) -> Value {
    build_pod_with(wf, ns, uid, step, None)
}

/// [`build_pod`] with the `/workspace` auto-mount (T-anyhome, Q36):
/// `Some(addr)` (the anyhome NFS `host:port`) appends the Workflow's shared
/// NFS volume + the [`workspace::MOUNT_TARGET`] volumeMount unless the user
/// already wired a workspace; `None` is byte-identical to [`build_pod`].
pub fn build_pod_with(
    wf: &Value,
    ns: &str,
    uid: &str,
    step: &PlannedStep,
    workspace_nfs: Option<&str>,
) -> Value {
    let wf_name = name(wf).unwrap_or_default();
    let labels = step_labels(wf_name, &step.template_name);
    let annotations = node_annotation(&step.node_name);
    let wf_spec = wf.pointer("/spec").unwrap_or(&Value::Null);
    let mut spec = build_pod_spec(wf_spec, &step.template);
    if let Some(addr) = workspace_nfs {
        inject_workspace(&mut spec, wf_spec, &step.template, wf_name, addr);
    }
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
        "spec": spec,
    })
}

/// The injected volume/mount name (T-anyhome, Q36). A user volume with this
/// literal name suppresses injection (see [`user_declares_workspace`]).
const WORKSPACE_VOLUME: &str = "workspace";

/// True when the USER already wired a `/workspace` themselves (A4: user
/// declaration always wins). Any of:
/// * a `container.volumeMounts[]` / `script.container.volumeMounts[]` entry
///   with `mountPath == workspace::MOUNT_TARGET`, or
/// * a `volumes[]` entry literally named `workspace` (workflow-level or
///   template-level).
fn user_declares_workspace(wf_spec: &Value, template: &Value) -> bool {
    let mounts_workspace = |container: Option<&Value>| {
        container
            .and_then(|c| c.get("volumeMounts"))
            .and_then(Value::as_array)
            .is_some_and(|mounts| {
                mounts.iter().any(|m| {
                    m.get("mountPath").and_then(Value::as_str) == Some(workspace::MOUNT_TARGET)
                })
            })
    };
    let names_workspace = |volumes: Option<&Value>| {
        volumes.and_then(Value::as_array).is_some_and(|volumes| {
            volumes
                .iter()
                .any(|v| v.get("name").and_then(Value::as_str) == Some(WORKSPACE_VOLUME))
        })
    };
    mounts_workspace(template.get("container"))
        || mounts_workspace(template.get("script").and_then(|s| s.get("container")))
        || names_workspace(wf_spec.get("volumes"))
        || names_workspace(template.get("volumes"))
}

/// Append the Workflow's SHARED workspace (T-anyhome, Q36) onto a built pod
/// spec: the NFS volume `{"name": "workspace", "nfs": {server, path
/// `/workflow/<wf>`, readOnly: false}}` after the wf+template concatenation
/// (array created when neither side declared volumes), plus the
/// `mountPath: MOUNT_TARGET` volumeMount. The ONE step container is shared
/// by the `container` and `script` template kinds (both compile to the
/// single "main" container), so one append injects both ("main 与 script
/// 容器都注入"). Skipped silently -- the pod stays user-shaped -- when the
/// user already declared a workspace (A4) or the workflow name fails the
/// SSOT path guard; never panics on odd shapes.
fn inject_workspace(
    spec: &mut Value,
    wf_spec: &Value,
    template: &Value,
    wf_name: &str,
    addr: &str,
) {
    if user_declares_workspace(wf_spec, template) {
        return; // A4: user declaration always wins.
    }
    let Some(path) = workspace::workflow_workspace_path(wf_name) else {
        return; // invalid workflow name: skip injection silently.
    };
    let Some(obj) = spec.as_object_mut() else {
        return;
    };
    let volumes = obj
        .entry("volumes".to_string())
        .or_insert_with(|| json!([]));
    if let Some(volumes) = volumes.as_array_mut() {
        volumes.push(json!({
            "name": WORKSPACE_VOLUME,
            "nfs": {"server": addr, "path": path, "readOnly": false},
        }));
    }
    if let Some(container) = obj
        .get_mut("containers")
        .and_then(Value::as_array_mut)
        .and_then(|containers| containers.first_mut())
        .and_then(Value::as_object_mut)
    {
        let mounts = container
            .entry("volumeMounts".to_string())
            .or_insert_with(|| json!([]));
        if let Some(mounts) = mounts.as_array_mut() {
            mounts.push(json!({
                "name": WORKSPACE_VOLUME,
                "mountPath": workspace::MOUNT_TARGET,
            }));
        }
    }
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
    use crate::controllers::workflow_plan::StepTarget;

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
                target: StepTarget::Pod,
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

    // ---- /workspace auto-mount injection (T-anyhome, Q36) ----

    fn step(template: Value) -> PlannedStep {
        PlannedStep {
            node_name: "wf[0].s".into(),
            template_name: "tmpl".into(),
            pod_name: "wf-0-s".into(),
            template,
            target: StepTarget::Pod,
        }
    }

    fn named_wf() -> Value {
        json!({"metadata": {"name": "wf"}, "spec": {}})
    }

    fn volume_names(pod: &Value) -> Vec<Value> {
        pod["spec"]["volumes"]
            .as_array()
            .map(|v| v.iter().map(|x| x["name"].clone()).collect())
            .unwrap_or_default()
    }

    #[test]
    fn workspace_injection_adds_nfs_volume_and_mount() {
        let template = json!({"container": {"image": "busybox", "command": ["true"]}});
        let pod = build_pod_with(
            &named_wf(),
            "default",
            "uid-1",
            &step(template),
            Some("10.0.0.5:2049"),
        );
        // Exactly one injected volume, appended after the (empty) concat.
        let volumes = pod["spec"]["volumes"].as_array().expect("volumes array");
        assert_eq!(volumes.len(), 1, "exactly the injected nfs volume");
        assert_eq!(volumes[0]["name"], "workspace");
        assert_eq!(volumes[0]["nfs"]["server"], "10.0.0.5:2049");
        assert_eq!(volumes[0]["nfs"]["path"], "/workflow/wf");
        assert_eq!(volumes[0]["nfs"]["readOnly"], false);
        // The mount rides the single "main" container.
        let mounts = pod["spec"]["containers"][0]["volumeMounts"]
            .as_array()
            .expect("volumeMounts array");
        assert_eq!(mounts.len(), 1);
        assert_eq!(mounts[0]["name"], "workspace");
        assert_eq!(mounts[0]["mountPath"], "/workspace");
    }

    #[test]
    fn workspace_injection_covers_script_templates() {
        // `script` compiles to the same single "main" container, so the
        // injection must land identically.
        let template = json!({"script": {"image": "python", "source": "print(1)"}});
        let pod = build_pod_with(
            &named_wf(),
            "default",
            "uid-1",
            &step(template),
            Some("anyhome.default.svc:2049"),
        );
        assert_eq!(pod["spec"]["containers"][0]["name"], "main");
        let volumes = pod["spec"]["volumes"].as_array().expect("volumes array");
        assert_eq!(volumes.len(), 1);
        assert_eq!(volumes[0]["nfs"]["path"], "/workflow/wf");
        let mounts = pod["spec"]["containers"][0]["volumeMounts"]
            .as_array()
            .expect("volumeMounts array");
        assert_eq!(mounts[0]["mountPath"], "/workspace");
    }

    #[test]
    fn workspace_injection_appends_after_user_volumes() {
        let template = json!({
            "container": {"image": "busybox", "volumeMounts": [{"name": "t", "mountPath": "/t"}]},
            "volumes": [{"name": "t"}],
        });
        let mut wf = named_wf();
        wf["spec"]["volumes"] = json!([{"name": "w"}]);
        let pod = build_pod_with(
            &wf,
            "default",
            "uid-1",
            &step(template),
            Some("10.0.0.5:2049"),
        );
        assert_eq!(
            volume_names(&pod),
            vec![json!("w"), json!("t"), json!("workspace")],
            "injected volume is APPENDED (wf then template, workspace last)"
        );
        let mounts = pod["spec"]["containers"][0]["volumeMounts"]
            .as_array()
            .expect("volumeMounts array");
        assert_eq!(
            mounts.iter().map(|m| m["name"].clone()).collect::<Vec<_>>(),
            vec![json!("t"), json!("workspace")]
        );
    }

    #[test]
    fn user_declared_workspace_mount_path_wins() {
        let template = json!({
            "container": {
                "image": "busybox",
                "volumeMounts": [{"name": "mine", "mountPath": "/workspace"}],
            },
            "volumes": [{"name": "mine"}],
        });
        let pod = build_pod_with(
            &named_wf(),
            "default",
            "uid-1",
            &step(template),
            Some("10.0.0.5:2049"),
        );
        assert_eq!(
            volume_names(&pod),
            vec![json!("mine")],
            "no injection: the user already mounts /workspace (A4)"
        );
        let mounts = pod["spec"]["containers"][0]["volumeMounts"]
            .as_array()
            .expect("volumeMounts array");
        assert_eq!(mounts.len(), 1);
        assert_eq!(mounts[0]["name"], "mine");
    }

    #[test]
    fn user_declared_script_mount_path_wins() {
        let template = json!({
            "script": {
                "image": "python",
                "source": "print(1)",
                "container": {
                    "volumeMounts": [{"name": "mine", "mountPath": "/workspace"}],
                },
            },
            "volumes": [{"name": "mine"}],
        });
        let pod = build_pod_with(
            &named_wf(),
            "default",
            "uid-1",
            &step(template),
            Some("10.0.0.5:2049"),
        );
        assert_eq!(volume_names(&pod), vec![json!("mine")]);
    }

    #[test]
    fn user_named_workspace_volume_wins() {
        // wf-level volume literally named "workspace".
        let template = json!({"container": {"image": "busybox"}});
        let mut wf = named_wf();
        wf["spec"]["volumes"] = json!([{"name": "workspace"}]);
        let pod = build_pod_with(
            &wf,
            "default",
            "uid-1",
            &step(template),
            Some("10.0.0.5:2049"),
        );
        assert_eq!(volume_names(&pod), vec![json!("workspace")]);
        assert!(pod["spec"]["containers"][0].get("volumeMounts").is_none());
        // Template-level volume named "workspace" suppresses too.
        let template = json!({
            "container": {"image": "busybox"},
            "volumes": [{"name": "workspace"}],
        });
        let pod = build_pod_with(
            &named_wf(),
            "default",
            "uid-1",
            &step(template),
            Some("10.0.0.5:2049"),
        );
        assert_eq!(volume_names(&pod), vec![json!("workspace")]);
    }

    #[test]
    fn workspace_none_is_the_pre_q36_builder() {
        let template = json!({"container": {"image": "busybox", "command": ["true"]}});
        let wf = named_wf();
        // Regression pin: None == the old build_pod output, and nothing
        // workspace-shaped appears anywhere.
        assert_eq!(
            build_pod_with(&wf, "default", "uid-1", &step(template.clone()), None),
            build_pod(&wf, "default", "uid-1", &step(template.clone()))
        );
        let pod = build_pod(&wf, "default", "uid-1", &step(template));
        assert!(pod["spec"].get("volumes").is_none());
        assert!(pod["spec"]["containers"][0].get("volumeMounts").is_none());
    }

    #[test]
    fn invalid_workflow_name_skips_injection_silently() {
        // Empty wf name fails the SSOT path guard: no injection, no panic.
        let template = json!({"container": {"image": "busybox"}});
        let wf = json!({"metadata": {"name": ""}, "spec": {}});
        let pod = build_pod_with(
            &wf,
            "default",
            "uid-1",
            &step(template),
            Some("10.0.0.5:2049"),
        );
        assert!(pod["spec"].get("volumes").is_none());
        assert!(pod["spec"]["containers"][0].get("volumeMounts").is_none());
    }
}
