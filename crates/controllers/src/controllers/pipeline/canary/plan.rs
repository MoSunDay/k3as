//! Pure canary-release decisions over raw JSON (TODO **T8.5**, decision
//! **Q58**).
//!
//! Side-effect free so the release state machine is unit-testable without
//! storage: `spec.release` parsing/validation, canary child names + label
//! scoping, backend weights, the per-step decision, child ownership and --
//! re-exported from [`super::children`] -- the canary Deployment/Service
//! clones with their drift-free merges. `super::roll` / `super::abort` only
//! sequence these decisions around storage effects.

use serde_json::{json, Map, Value};

use crate::object::controller_of;
use crate::time::parse_rfc3339;

use super::super::plan::{slug, str_at, truncate_slug};
use super::super::PIPELINE_KIND;

pub(super) use super::children::{
    clone_deployment, clone_service, merge_deployment, merge_service,
};

/// Pipeline name label (every canary child carries it).
pub(super) const PIPELINE_LABEL: &str = "init-pro.io/pipeline";
/// Canary marker label.
pub(super) const CANARY_LABEL: &str = "init-pro.io/canary";
/// Canary revision label (bounded slug of the tag).
const REVISION_LABEL: &str = "init-pro.io/revision";
/// DNS-1123 label ceiling for every name the release owns.
const MAX_NAME_CHARS: usize = 63;
/// Revision label value ceiling.
const MAX_REVISION_CHARS: usize = 40;

/// The resolved shape of one canary release (names + the revision to ship).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ReleasePlan {
    /// Stable backend Service (operator-owned, must exist).
    pub stable_service: String,
    /// Canary backend Service the pipeline owns (`<stableService>-canary`).
    pub canary_service: String,
    /// Canary Deployment the pipeline owns (`<targetDeployment>-canary`).
    pub canary_deployment: String,
    /// `TrafficSplit` the release steps (must exist).
    pub traffic_split: String,
    /// Stable Deployment the canary mirrors (empty on the abort-only path).
    pub stable_deployment: String,
    /// Container whose image the canary replaces (empty = first container).
    pub container: String,
    /// Weight steps in order.
    pub steps: Vec<api::pipeline::ReleaseStep>,
    /// Image reference the canary revision runs.
    pub revision: String,
    /// `init-pro.io/revision` label value.
    pub revision_label: String,
}

/// Per-pass decision of the canary state machine (pure).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum StepDecision {
    /// Publish `steps[step].weight` now.
    Apply { step: usize },
    /// Hold the current weight until its soak elapses.
    Wait { step: usize },
    /// All steps soaked: promote the canary image into the stable Deployment.
    Promote,
}

/// `spec.release` as a typed object (`None` when absent or non-object).
pub(super) fn parse_release(sp: &Value) -> Option<api::pipeline::PipelineRelease> {
    let raw = sp.pointer("/spec/release")?;
    raw.is_object()
        .then(|| serde_json::from_value(raw.clone()).ok())
        .flatten()
}

/// Names-only validation (the abort path: no steps/target required).
pub(super) fn validate_names(
    sp: &Value,
    release: &api::pipeline::PipelineRelease,
) -> Result<ReleasePlan, String> {
    let split = release.traffic_split.trim();
    let stable = release.stable_service.trim();
    if stable.is_empty() {
        return Err("spec.release.stableService is required".into());
    }
    let target = str_at(sp, "/spec/target/deployment").trim();
    let base = if target.is_empty() { stable } else { target };
    let canary_service = with_default(&release.canary_service, || format!("{stable}-canary"));
    let canary_deployment = with_default(&release.canary_deployment, || format!("{base}-canary"));
    validate_name("spec.release.trafficSplit", split)?;
    validate_name("spec.release.stableService", stable)?;
    validate_name("spec.release.canaryService", &canary_service)?;
    validate_name("spec.release.canaryDeployment", &canary_deployment)?;
    if !target.is_empty() {
        validate_name("spec.target.deployment", target)?;
    }
    if canary_service == stable {
        return Err(format!(
            "spec.release.canaryService must differ from {stable}"
        ));
    }
    if !target.is_empty() && canary_deployment == target {
        return Err(format!(
            "spec.release.canaryDeployment must differ from {target}"
        ));
    }
    Ok(ReleasePlan {
        stable_service: stable.to_string(),
        canary_service,
        canary_deployment,
        traffic_split: split.to_string(),
        stable_deployment: target.to_string(),
        container: str_at(sp, "/spec/target/container").trim().to_string(),
        steps: release.steps.clone(),
        revision: image_reference(sp),
        revision_label: revision_label(str_at(sp, "/spec/image/tag")),
    })
}

/// Full validation for the roll path: names + target + a non-empty step
/// list whose weights all sit within `0..=100`.
pub(super) fn validate(
    sp: &Value,
    release: &api::pipeline::PipelineRelease,
    reference: &str,
    tag: &str,
) -> Result<ReleasePlan, String> {
    let mut plan = validate_names(sp, release)?;
    if plan.stable_deployment.is_empty() {
        return Err("spec.target.deployment is required for a canary release".into());
    }
    if release.steps.is_empty() {
        return Err("spec.release.steps must contain at least one weight step".into());
    }
    if let Some((index, step)) = release
        .steps
        .iter()
        .enumerate()
        .find(|(_, step)| !(0..=100).contains(&step.weight))
    {
        return Err(format!(
            "spec.release.steps[{index}].weight must be within 0..=100 (got {})",
            step.weight
        ));
    }
    if !reference.trim().is_empty() {
        plan.revision = reference.trim().to_string();
    }
    plan.revision_label = revision_label(tag);
    Ok(plan)
}

/// Pod labels for the canary: the stable pod labels MINUS every stable
/// selector key (the canary's selector must not collide with the immutable
/// stable one), plus the canary identity labels.
pub(super) fn canary_labels(
    pipeline: &str,
    stable_pod_labels: &Value,
    stable_selector: &Value,
    revision_label: &str,
) -> Map<String, Value> {
    let mut labels = stable_pod_labels.as_object().cloned().unwrap_or_default();
    if let Some(selector) = stable_selector.as_object() {
        for key in selector.keys() {
            labels.remove(key);
        }
    }
    labels.insert(PIPELINE_LABEL.to_string(), json!(pipeline));
    labels.insert(CANARY_LABEL.to_string(), json!("true"));
    labels.insert(REVISION_LABEL.to_string(), json!(revision_label));
    labels
}

/// The canary selector: identity labels only (NOT the revision label -- the
/// selector is immutable, the revision is not).
pub(super) fn selector_labels(pipeline: &str) -> Value {
    let mut labels = Map::new();
    labels.insert(PIPELINE_LABEL.to_string(), json!(pipeline));
    labels.insert(CANARY_LABEL.to_string(), json!("true"));
    Value::Object(labels)
}

/// `TrafficSplit.spec.backends` for canary `weight` (clamped to `0..=100`).
pub(super) fn desired_backends(plan: &ReleasePlan, weight: i32) -> Vec<api::traffic::SplitBackend> {
    let canary = weight.clamp(0, 100);
    let backend = |service: &String, weight: i32| api::traffic::SplitBackend {
        service: service.clone(),
        weight,
    };
    vec![
        backend(&plan.stable_service, 100 - canary),
        backend(&plan.canary_service, canary),
    ]
}

/// The next move of the canary state machine (`now_secs` = wall clock).
pub(super) fn step_decision(
    rollout: Option<&api::pipeline::RolloutStatus>,
    plan: &ReleasePlan,
    now_secs: u64,
) -> StepDecision {
    let Some(last) = plan.steps.len().checked_sub(1) else {
        return StepDecision::Promote; // no steps: nothing to soak
    };
    let Some(rollout) = rollout else {
        return StepDecision::Apply { step: 0 };
    };
    if rollout.revision != plan.revision || rollout.step < 0 {
        return StepDecision::Apply { step: 0 }; // new revision: restart
    }
    let step = rollout.step as usize;
    if step > last {
        return StepDecision::Promote;
    }
    let since = parse_rfc3339(&rollout.since).unwrap_or(now_secs);
    if now_secs.saturating_sub(since) < u64::from(plan.steps[step].pause_seconds) {
        return StepDecision::Wait { step };
    }
    match step < last {
        true => StepDecision::Apply { step: step + 1 },
        false => StepDecision::Promote,
    }
}

/// True when `child` carries a controller ownerReference naming THIS
/// pipeline (uid compared only when both sides carry one).
pub(super) fn owned_by(child: &Value, pipeline: &str, uid: &str) -> bool {
    let Some(owner) = controller_of(child) else {
        return false;
    };
    let (kind, name, owner_uid) = (
        text_at(&owner, "kind"),
        text_at(&owner, "name"),
        text_at(&owner, "uid"),
    );
    kind == PIPELINE_KIND
        && name == pipeline
        && (uid.is_empty() || owner_uid.is_empty() || owner_uid == uid)
}

/// `object[field]` as a string (empty when absent).
fn text_at(object: &Value, field: &str) -> String {
    object
        .get(field)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// Trimmed value, or the computed default when empty.
fn with_default(value: &str, fallback: impl FnOnce() -> String) -> String {
    match value.trim() {
        "" => fallback(),
        given => given.to_string(),
    }
}

/// DNS-1123 label check, with the offending field named in the error.
fn validate_name(field: &str, name: &str) -> Result<(), String> {
    let size = name.chars().count();
    let bad_edge = name.starts_with('-') || name.ends_with('-');
    if name.is_empty() || size > MAX_NAME_CHARS || bad_edge || !name.bytes().all(is_name_byte) {
        return Err(format!(
            "{field} must be a 1..={MAX_NAME_CHARS} char [a-z0-9-] name (got {name:?})"
        ));
    }
    Ok(())
}

/// `[a-z0-9-]` membership test for one byte.
fn is_name_byte(byte: u8) -> bool {
    byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'
}

/// `init-pro.io/revision` value: the slugged tag, bounded, never empty.
fn revision_label(tag: &str) -> String {
    match truncate_slug(&slug(tag), MAX_REVISION_CHARS) {
        label if label.is_empty() => "rev".to_string(),
        label => label,
    }
}

/// `repository:tag` as the controllers spell it (empty halves tolerated).
fn image_reference(sp: &Value) -> String {
    let repo = str_at(sp, "/spec/image/repository").trim();
    let tag = str_at(sp, "/spec/image/tag").trim();
    match (repo.trim_end_matches(':'), tag) {
        ("", "") => String::new(),
        ("", tag) => tag.to_string(),
        (repo, "") => repo.to_string(),
        (repo, tag) => format!("{repo}:{tag}"),
    }
}

#[cfg(test)]
mod tests;
