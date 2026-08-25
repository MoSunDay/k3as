//! Pure Workflow plan compilation (TODO **T7.3 Scope B**, decision **Q30**).
//!
//! Compiles a Workflow's entrypoint template into an execution plan the
//! controller can drive level-by-level: [`Plan`] is a sequence of parallel
//! step groups -- a `steps` template yields one group per outer array
//! element, a `dag` template is layered topologically (Kahn's algorithm
//! over `dependencies` / `depends`, e.g. `"A && B"` with optional
//! `.Succeeded`-style result suffixes), and a plain container/script
//! entrypoint yields a single one-step group. Everything here is pure
//! (no I/O), so the semantics are unit-testable in isolation.
//!
//! Unsupported constructs (suspend/http/resource/plugin templates, missing
//! references, dependency cycles) surface as `None` via [`build_plan`] --
//! or with the reason via [`compile`], which the reconciler reports in
//! `status.message` while parking the workflow at Pending (Q10).

use serde_json::Value;

use super::workflow_dag::dag_groups;
use super::workflow_phase::pod_name_for;

/// Root node types (upstream `NodeType` values we emit).
pub const ROOT_STEPS: &str = "Steps";
pub const ROOT_DAG: &str = "DAG";
pub const ROOT_POD: &str = "Pod";

/// One executable unit: a resolved template plus its node/pod identity.
#[derive(Clone, Debug)]
pub struct PlannedStep {
    /// Argo node-tree name (`<wf>[<g>].<step>` / `<wf>.<task>` / `<wf>`).
    /// Doubles as the node ID -- see the controller module doc for the
    /// simplification.
    pub node_name: String,
    /// Name of the resolved template (`status.nodes[].templateName`).
    pub template_name: String,
    /// Deterministic DNS-1123 pod name ([`pod_name_for`]).
    pub pod_name: String,
    /// The resolved template (always container/script; validated).
    pub template: Value,
}

/// A compiled execution plan: sequential groups of parallel steps.
#[derive(Clone, Debug)]
pub struct Plan {
    pub root_type: &'static str,
    pub groups: Vec<Vec<PlannedStep>>,
}

/// Compile the plan, or the reason the workflow cannot be executed (the
/// reconciler surfaces the reason in `status.message`).
pub(crate) fn compile(wf: &Value) -> Result<Plan, String> {
    let wf_name = wf
        .pointer("/metadata/name")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or("workflow has no metadata.name")?;
    let entrypoint = wf
        .pointer("/spec/entrypoint")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or("spec.entrypoint is not set")?;
    let templates = wf
        .pointer("/spec/templates")
        .and_then(Value::as_array)
        .filter(|t| !t.is_empty())
        .ok_or("spec.templates is missing or empty")?;
    let entry = find_template(templates, entrypoint)
        .ok_or_else(|| format!("entrypoint template '{entrypoint}' not found"))?;

    if let Some(groups) = steps_groups(wf_name, entry, templates)? {
        return Ok(Plan {
            root_type: ROOT_STEPS,
            groups,
        });
    }
    if let Some(groups) = dag_groups(wf_name, entry, templates)? {
        return Ok(Plan {
            root_type: ROOT_DAG,
            groups,
        });
    }
    if is_executable(entry) {
        // Plain container/script entrypoint: the root node IS the pod node.
        return Ok(Plan {
            root_type: ROOT_POD,
            groups: vec![vec![PlannedStep {
                node_name: wf_name.to_string(),
                template_name: entrypoint.to_string(),
                pod_name: pod_name_for(wf_name, wf_name),
                template: entry.clone(),
            }]],
        });
    }
    Err(unsupported(entrypoint, entry))
}

/// [`compile`] as a boolean (None = unsupported/unparseable spec).
pub fn build_plan(wf: &Value) -> Option<Plan> {
    compile(wf).ok()
}

/// `steps: [[...], ...]` -> one group per outer element.
/// Look up a named template (returns None when `spec.templates` lacks it).
fn find_template<'a>(templates: &'a [Value], name: &str) -> Option<&'a Value> {
    templates
        .iter()
        .find(|t| t.get("name").and_then(Value::as_str) == Some(name))
}

fn steps_groups(
    wf_name: &str,
    entry: &Value,
    templates: &[Value],
) -> Result<Option<Vec<Vec<PlannedStep>>>, String> {
    let Some(steps) = entry.get("steps").and_then(Value::as_array) else {
        return Ok(None);
    };
    let mut groups = Vec::with_capacity(steps.len());
    for (g, group) in steps.iter().enumerate() {
        let inner = group
            .as_array()
            .ok_or_else(|| format!("steps[{g}] is not a parallel group (array)"))?;
        let mut planned = Vec::with_capacity(inner.len());
        for step in inner {
            let where_ = format!("steps[{g}]");
            let (template_name, template) = resolve(&where_, step, templates)?;
            if !is_executable(&template) {
                return Err(unsupported(&template_name, &template));
            }
            let label = step
                .get("name")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .unwrap_or(template_name.as_str());
            let node_name = format!("{wf_name}[{g}].{label}");
            planned.push(PlannedStep {
                pod_name: pod_name_for(wf_name, &node_name),
                node_name,
                template_name,
                template,
            });
        }
        groups.push(planned);
    }
    Ok(Some(groups))
}

/// `dag.tasks` layered topologically (Kahn, declaration order within a
/// layer for determinism).
/// Resolve a step/task's target template: `inline` verbatim, or the named
/// template from `spec.templates`.
pub(crate) fn resolve(
    where_: &str,
    reference: &Value,
    templates: &[Value],
) -> Result<(String, Value), String> {
    if let Some(inline) = reference.get("inline").filter(|v| v.as_object().is_some()) {
        let name = reference
            .get("template")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .or_else(|| inline.get("name").and_then(Value::as_str))
            .unwrap_or("inline");
        return Ok((name.to_string(), inline.clone()));
    }
    let tname = reference
        .get("template")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("{where_} references no template"))?;
    let t = find_template(templates, tname)
        .ok_or_else(|| format!("{where_} references missing template '{tname}'"))?;
    Ok((tname.to_string(), t.clone()))
}

/// Only container/script templates are executable in v1.
pub(crate) fn is_executable(t: &Value) -> bool {
    t.get("container").is_some() || t.get("script").is_some()
}

/// Human-readable rejection naming the unsupported construct.
pub(crate) fn unsupported(name: &str, t: &Value) -> String {
    let construct = [
        "suspend",
        "http",
        "resource",
        "plugin",
        "containerSet",
        "data",
    ]
    .into_iter()
    .chain(["steps", "dag"])
    .find(|k| t.get(k).is_some())
    .unwrap_or("non-container");
    format!("template '{name}' uses unsupported construct '{construct}'")
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn steps_wf() -> Value {
        json!({"metadata": {"name": "two-step"}, "spec": {
            "entrypoint": "main",
            "templates": [
                {"name": "main", "steps": [
                    [{"name": "gen-a", "template": "tmpl-a"}],
                    [{"name": "gen-b", "template": "tmpl-b"},
                     {"name": "gen-c", "template": "tmpl-b"}]]},
                {"name": "tmpl-a", "container": {"image": "busybox"}},
                {"name": "tmpl-b", "script": {"image": "busybox", "source": "echo hi"}}]}})
    }

    #[test]
    fn linear_and_parallel_steps_plan() {
        let plan = build_plan(&steps_wf()).unwrap();
        assert_eq!(plan.root_type, ROOT_STEPS);
        assert_eq!(plan.groups.len(), 2);
        let g0 = &plan.groups[0];
        assert_eq!(
            (g0[0].node_name.as_str(), g0[0].pod_name.as_str()),
            ("two-step[0].gen-a", "two-step-0-gen-a")
        );
        // Parallel group: both steps share the group index.
        let g1 = &plan.groups[1];
        assert_eq!(
            (g1.len(), g1[0].node_name.as_str(), g1[1].node_name.as_str()),
            (2, "two-step[1].gen-b", "two-step[1].gen-c")
        );
        assert_eq!(g1[0].pod_name, "two-step-1-gen-b");
    }

    #[test]
    fn container_only_plan_is_single_pod_root() {
        let wf = json!({
            "metadata": {"name": "single"},
            "spec": {
                "entrypoint": "main",
                "templates": [{"name": "main", "container": {"image": "busybox"}}],
            },
        });
        let plan = build_plan(&wf).unwrap();
        assert_eq!(plan.root_type, ROOT_POD);
        assert_eq!(plan.groups.len(), 1);
        assert_eq!(plan.groups[0].len(), 1);
        assert_eq!(
            (
                plan.groups[0][0].node_name.as_str(),
                plan.groups[0][0].pod_name.as_str()
            ),
            ("single", "single")
        );
    }

    #[test]
    fn unsupported_constructs_are_rejected_with_reason() {
        let base = |entry: Value| {
            json!({"metadata": {"name": "wf"}, "spec": {
                "entrypoint": "main", "templates": [entry, {"name": "c", "container": {"image": "i"}}],
            }})
        };
        assert!(build_plan(&base(json!({"name": "main", "suspend": {}}))).is_none());
        assert!(build_plan(&base(json!({"name": "main", "http": {"url": "x"}}))).is_none());
        // A step resolving to a non-container template is rejected too.
        let wf = json!({"metadata": {"name": "wf"}, "spec": {"entrypoint": "main",
            "templates": [
                {"name": "main", "steps": [[{"name": "s", "template": "susp"}]]},
                {"name": "susp", "suspend": {}}]}});
        assert!(build_plan(&wf).is_none());
        let wf = json!({"metadata": {"name": "wf"}, "spec": {"entrypoint": "main",
            "templates": [{"name": "main", "suspend": {}}]}});
        assert_eq!(
            compile(&wf).unwrap_err(),
            "template 'main' uses unsupported construct 'suspend'"
        );
        // Missing entrypoint template / missing entrypoint.
        let wf = json!({"metadata": {"name": "wf"}, "spec": {"entrypoint": "nope",
            "templates": [{"name": "main", "container": {"image": "i"}}]}});
        assert!(compile(&wf).unwrap_err().contains("not found"));
        let wf = json!({"metadata": {"name": "wf"}, "spec":
            {"templates": [{"name": "main", "container": {"image": "i"}}]}});
        assert!(compile(&wf).unwrap_err().contains("entrypoint"));
        let cyc = json!({"metadata": {"name": "wf"}, "spec": {
        "entrypoint": "main", "templates": [{"name": "main", "dag": {"tasks": [
            {"name": "A", "template": "main", "dependencies": ["B"]},
            {"name": "B", "template": "main", "dependencies": ["A"]},
        ]}}]}});
        assert!(compile(&cyc).unwrap_err().contains("cycle"));
    }
}
