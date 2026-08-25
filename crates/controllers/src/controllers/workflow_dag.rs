//! DAG template compilation (TODO **T7.3 Scope B**, decision **Q30**).
//!
//! Layers a `dag` template's tasks into the parallel groups [`Plan`]
//! consumes: Kahn's algorithm emits every unplaced task whose dependencies
//! are all satisfied as one layer, repeating until exhausted; leftover
//! tasks mean a dependency cycle (a compile error, surfaced via
//! [`compile`](super::workflow_plan::compile) as `status.message`).
//! Dependencies come from both `dependencies: [A, B]` and the parsed
//! `depends: "A && B"` form, with `.Succeeded`-style result suffixes
//! stripped. Pure, no I/O (Q10).

use serde_json::Value;

use super::workflow_phase::pod_name_for;
use super::workflow_plan::{is_executable, resolve, unsupported, PlannedStep};

pub(super) fn dag_groups(
    wf_name: &str,
    entry: &Value,
    templates: &[Value],
) -> Result<Option<Vec<Vec<PlannedStep>>>, String> {
    let Some(tasks) = entry.pointer("/dag/tasks").and_then(Value::as_array) else {
        return Ok(None);
    };
    let mut names: Vec<&str> = Vec::with_capacity(tasks.len());
    for t in tasks {
        let Some(nm) = t
            .get("name")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        else {
            return Err("dag task without a name".into());
        };
        if names.contains(&nm) {
            return Err(format!("duplicate dag task name '{nm}'"));
        }
        names.push(nm);
    }
    let deps: Vec<Vec<usize>> = tasks
        .iter()
        .map(|t| task_deps(&names, t))
        .collect::<Result<_, _>>()?;

    // Kahn: repeatedly emit every unplaced task whose deps are all placed.
    let mut unplaced = vec![true; tasks.len()];
    let mut layers: Vec<Vec<usize>> = Vec::new();
    loop {
        let layer: Vec<usize> = (0..tasks.len())
            .filter(|&i| unplaced[i] && deps[i].iter().all(|&d| !unplaced[d]))
            .collect();
        if layer.is_empty() {
            break;
        }
        for &i in &layer {
            unplaced[i] = false;
        }
        layers.push(layer);
    }
    if unplaced.iter().any(|&u| u) {
        return Err("dag contains a dependency cycle".into());
    }

    let mut groups = Vec::with_capacity(layers.len());
    for layer in layers {
        let mut planned = Vec::with_capacity(layer.len());
        for i in layer {
            let task = &tasks[i];
            let where_ = format!("dag task '{}'", names[i]);
            let (template_name, template) = resolve(&where_, task, templates)?;
            if !is_executable(&template) {
                return Err(unsupported(&template_name, &template));
            }
            let node_name = format!("{wf_name}.{}", names[i]);
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

/// Dependency indices of one task: `dependencies: [..]` plus the parsed
/// `depends: "A && B"` form (`.Succeeded`-style result suffixes stripped).
fn task_deps(names: &[&str], task: &Value) -> Result<Vec<usize>, String> {
    let mut out: Vec<usize> = Vec::new();
    let mut add = |raw: &str| -> Result<(), String> {
        let base = raw.trim();
        let base = base.rsplit_once('.').map(|(n, _)| n).unwrap_or(base);
        let Some(idx) = names.iter().position(|n| *n == base) else {
            return Err(format!("task depends on unknown task '{raw}'"));
        };
        if !out.contains(&idx) {
            out.push(idx);
        }
        Ok(())
    };
    for d in task
        .get("dependencies")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(s) = d.as_str() {
            add(s)?;
        }
    }
    if let Some(depends) = task.get("depends").and_then(Value::as_str) {
        for part in depends.split("&&") {
            add(part)?;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::super::workflow_plan::{build_plan, ROOT_DAG};
    use serde_json::json;

    #[test]
    fn dag_diamond_layers() {
        let wf = json!({
            "metadata": {"name": "diamond"},
            "spec": {
                "entrypoint": "main",
                "templates": [
                    {"name": "main", "dag": {"tasks": [
                        {"name": "A", "template": "work"},
                        {"name": "B", "template": "work", "dependencies": ["A"]},
                        {"name": "C", "template": "work", "depends": "A"},
                        {"name": "D", "template": "work", "depends": "B && C"},
                    ]}},
                    {"name": "work", "container": {"image": "busybox"}},
                ],
            },
        });
        let plan = build_plan(&wf).unwrap();
        assert_eq!(plan.root_type, ROOT_DAG);
        let names: Vec<Vec<String>> = plan
            .groups
            .iter()
            .map(|g| g.iter().map(|s| s.node_name.clone()).collect())
            .collect();
        assert_eq!(
            names,
            vec![
                vec!["diamond.A"],
                vec!["diamond.B", "diamond.C"],
                vec!["diamond.D"],
            ]
        );
    }
}
