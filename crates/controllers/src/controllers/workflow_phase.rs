//! Pure Workflow naming + phase math (TODO **T7.3 Scope B**, decision
//! **Q30**).
//!
//! The deterministic pod-name projection (node name -> DNS-1123 subdomain)
//! and the pod -> node/workflow phase reductions the controller composes
//! its status from. Pure and side-effect-free so the semantics are
//! unit-testable in isolation. JSON-only wire (Q10).

use serde_json::Value;

/// Workflow/node phases the reductions reason over (upstream `NodePhase`).
pub const PHASE_PENDING: &str = "Pending";
pub const PHASE_RUNNING: &str = "Running";
pub const PHASE_SUCCEEDED: &str = "Succeeded";
pub const PHASE_FAILED: &str = "Failed";

/// Deterministic DNS-1123 subdomain pod name for a node: lowercase, every
/// run of chars outside `[a-z0-9-]` collapses to a single `-` (node names
/// already start with the workflow name, so nothing is prefixed), capped
/// at 63 with trailing `-` trimmed.
pub fn pod_name_for(wf_name: &str, node_name: &str) -> String {
    debug_assert!(
        node_name.starts_with(wf_name),
        "node name must be rooted in the workflow name"
    );
    let mut out = String::with_capacity(node_name.len());
    for c in node_name.chars() {
        match c {
            'a'..='z' | '0'..='9' => out.push(c),
            'A'..='Z' => out.push(c.to_ascii_lowercase()),
            _ => {
                if !out.is_empty() && !out.ends_with('-') {
                    out.push('-');
                }
            }
        }
    }
    out.truncate(63);
    while out.ends_with('-') {
        out.pop();
    }
    out
}

/// Node phase implied by a pod's `status.phase` (missing/Unknown -> Pending:
/// v1 has no kubelet-side Unknown mapping).
pub fn node_phase_from_pod(pod: &Value) -> &'static str {
    match pod.pointer("/status/phase").and_then(Value::as_str) {
        Some(PHASE_RUNNING) => PHASE_RUNNING,
        Some(PHASE_SUCCEEDED) => PHASE_SUCCEEDED,
        Some(PHASE_FAILED) => PHASE_FAILED,
        _ => PHASE_PENDING,
    }
}

/// Workflow phase over the per-step phases: any Failed/Error -> Failed;
/// all Succeeded -> Succeeded (vacuously true for zero steps); otherwise
/// Running once any pod exists, Pending while none does.
pub fn workflow_phase(step_phases: &[&str], any_pod_exists: bool) -> &'static str {
    if step_phases
        .iter()
        .all(|p| matches!(*p, PHASE_SUCCEEDED | "Skipped"))
    {
        return PHASE_SUCCEEDED;
    }
    if step_phases
        .iter()
        .any(|p| matches!(*p, PHASE_FAILED | "Error"))
    {
        return PHASE_FAILED;
    }
    if any_pod_exists {
        PHASE_RUNNING
    } else {
        PHASE_PENDING
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn pod_name_sanitization() {
        assert_eq!(
            pod_name_for("two-step", "two-step[0].gen-a"),
            "two-step-0-gen-a"
        );
        assert_eq!(pod_name_for("wf", "wf.A"), "wf-a");
        assert_eq!(pod_name_for("wf", "wf.Boot"), "wf-boot");
        let long = format!("wf-{}.a", "x".repeat(80));
        assert!(pod_name_for("wf", &long).len() <= 63);
        assert!(!pod_name_for("wf", &long).ends_with('-'));
    }

    #[test]
    fn phase_math() {
        assert_eq!(workflow_phase(&[], false), PHASE_SUCCEEDED); // vacuous
        assert_eq!(workflow_phase(&["Pending"], false), PHASE_PENDING);
        assert_eq!(workflow_phase(&["Pending"], true), PHASE_RUNNING);
        assert_eq!(workflow_phase(&["Running", "Pending"], true), PHASE_RUNNING);
        assert_eq!(workflow_phase(&["Succeeded", "Failed"], true), PHASE_FAILED);
        assert_eq!(workflow_phase(&["Succeeded", "Error"], true), PHASE_FAILED);
        assert_eq!(workflow_phase(&["Succeeded"], true), PHASE_SUCCEEDED);
        assert_eq!(
            node_phase_from_pod(&json!({"status": {"phase": "Running"}})),
            PHASE_RUNNING
        );
        assert_eq!(node_phase_from_pod(&json!({})), PHASE_PENDING);
    }
}
