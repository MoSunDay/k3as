//! Canary release controller (TODO **T8.5**, decision **Q58**): the
//! pipeline-owned rollout face of a `ServicePipeline`.
//!
//! When `spec.release` is set, a `Succeeded` build no longer patches the
//! stable Deployment directly. [`roll`] instead clones the stable Deployment
//! into `<target>-canary` (revision image), clones the stable Service into
//! `<stableService>-canary` (ClusterIP, no node port), and walks
//! `spec.release.steps`: step `i` publishes its weight on the `TrafficSplit`
//! (stable = `100 - w`, canary = `w`), soaks `pauseSeconds`, then advances.
//! After the last soak the canary image is promoted into the stable
//! Deployment and the split returns to `stable 100 / canary 0`.
//!
//! [`abort`] (`spec.release.abort: true`) deletes the canary children and
//! resets the split; the stable image is never touched.
//!
//! Status (full-object CAS, Q10): `Rolling` carries
//! `status.rollout {step, weight, since, revision}`; `observedTag` only
//! advances on promote (abort keeps it) and on a `Failed` release, so the
//! failed tag is HELD instead of retried on every resync. Every failure mode
//! reports `Failed` with a named `Ready=False` reason (`ReleaseInvalid`,
//! `TrafficSplitMissing`, `StableDeploymentMissing`, `StableServiceMissing`,
//! `CanaryConflict`) -- and only on the pass that first observes it: the
//! 2s resync re-enters [`roll`] forever, so [`fail`] writes once per
//! distinct failure instead of minting a fresh timestamp every pass.

mod apply;
mod children;
mod plan;
#[cfg(test)]
mod tests;

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use storage::Key;

use crate::client::Client;
use crate::error::ControllerError;
use crate::object::owner_reference;

use super::status::{condition, now, status_of, write_status};
use super::{patch_target_deployment, plan as controller_plan, PIPELINE_KIND};

/// True when `spec.release` is a JSON object (canary rollout configured).
pub(super) fn has_release(sp: &Value) -> bool {
    sp.pointer("/spec/release")
        .map(Value::is_object)
        .unwrap_or(false)
}

/// True when the operator asked to abort the in-flight canary.
pub(super) fn abort_requested(sp: &Value) -> bool {
    sp.pointer("/spec/release/abort").and_then(Value::as_bool) == Some(true)
}

/// Drive (or continue) the canary rollout for a freshly published revision.
#[allow(clippy::too_many_arguments)] // the pipeline identity is the call site's
pub(super) async fn roll(
    client: &Arc<dyn Client>,
    key: &Key,
    sp: &Value,
    latest: &Value,
    ns: &str,
    pipeline: &str,
    uid: &str,
    tag: &str,
    reference: &str,
) -> Result<(), ControllerError> {
    let Some(release) = plan::parse_release(sp) else {
        if !has_release(sp) {
            return Ok(()); // no canary config: nothing to roll
        }
        return fail(
            client,
            key,
            sp,
            "ReleaseInvalid",
            "spec.release is not a PipelineRelease",
            Some(tag),
        )
        .await;
    };
    let plan = match plan::validate(sp, &release, reference, tag) {
        Ok(plan) => plan,
        Err(message) => return fail(client, key, sp, "ReleaseInvalid", &message, Some(tag)).await,
    };
    let split_key = apply::split_key(ns, &plan.traffic_split);
    if apply::get(client, &split_key).await?.is_none() {
        return fail(
            client,
            key,
            sp,
            "TrafficSplitMissing",
            &missing("TrafficSplit", ns, &plan.traffic_split),
            Some(tag),
        )
        .await;
    }
    let stable_key = Key::new("apps", "deployments", ns, &plan.stable_deployment);
    let Some(stable_dep) = apply::get(client, &stable_key).await? else {
        return fail(
            client,
            key,
            sp,
            "StableDeploymentMissing",
            &missing("Deployment", ns, &plan.stable_deployment),
            Some(tag),
        )
        .await;
    };
    let stable_key = Key::new("", "services", ns, &plan.stable_service);
    let Some(stable_svc) = apply::get(client, &stable_key).await? else {
        return fail(
            client,
            key,
            sp,
            "StableServiceMissing",
            &missing("Service", ns, &plan.stable_service),
            Some(tag),
        )
        .await;
    };
    // Deployment first: its pods must be selectable before the Service exists.
    let labels = plan::canary_labels(
        pipeline,
        stable_dep
            .pointer("/spec/template/metadata/labels")
            .unwrap_or(&Value::Null),
        stable_dep
            .pointer("/spec/selector/matchLabels")
            .unwrap_or(&Value::Null),
        &plan.revision_label,
    );
    let owner = owner_reference(pipeline, uid, PIPELINE_KIND, api::pipeline::API_VERSION);
    let dep_key = Key::new("apps", "deployments", ns, &plan.canary_deployment);
    let desired = plan::clone_deployment(&stable_dep, &plan, &labels, owner.clone());
    if apply::ensure_deployment(client, &dep_key, desired, pipeline, uid).await?
        == apply::Ensured::Foreign
    {
        return fail(
            client,
            key,
            sp,
            "CanaryConflict",
            &conflict("Deployment", ns, &plan.canary_deployment),
            Some(tag),
        )
        .await;
    }
    let svc_key = Key::new("", "services", ns, &plan.canary_service);
    let desired = plan::clone_service(&stable_svc, &plan, &labels, owner);
    if apply::ensure_service(client, &svc_key, desired, pipeline, uid).await?
        == apply::Ensured::Foreign
    {
        return fail(
            client,
            key,
            sp,
            "CanaryConflict",
            &conflict("Service", ns, &plan.canary_service),
            Some(tag),
        )
        .await;
    }
    match plan::step_decision(rollout_of(sp).as_ref(), &plan, now_secs()) {
        plan::StepDecision::Apply { step } => {
            let weight = plan.steps[step].weight.clamp(0, 100);
            apply::patch_split(client, &split_key, plan::desired_backends(&plan, weight)).await?;
            let ts = now();
            let mut state = status_of(sp);
            state["phase"] = json!(api::pipeline::PHASE_ROLLING);
            state["rollout"] = json!({
                "step": step,
                "weight": weight,
                "since": ts,
                "revision": plan.revision,
            });
            state["conditions"] = json!([condition(
                "Ready",
                false,
                "Progressing",
                &format!("canary step {}/{} at {weight}%", step + 1, plan.steps.len()),
                &ts
            )]);
            write_status(client, key, sp, state).await.map(|_| ())
        }
        plan::StepDecision::Wait { .. } => {
            if status_of(sp).get("phase").and_then(Value::as_str)
                == Some(api::pipeline::PHASE_ROLLING)
            {
                return Ok(()); // soaking: `write_status` would no-op anyway
            }
            let mut state = status_of(sp);
            state["phase"] = json!(api::pipeline::PHASE_ROLLING);
            write_status(client, key, sp, state).await.map(|_| ())
        }
        plan::StepDecision::Promote => {
            patch_target_deployment(client, ns, sp, reference).await?;
            apply::patch_split(client, &split_key, plan::desired_backends(&plan, 0)).await?;
            let ts = now();
            let mut state = status_of(sp);
            state["phase"] = json!(api::pipeline::PHASE_PROMOTED);
            state["observedTag"] = json!(tag);
            state["lastBuild"] = controller_plan::build_ref(latest);
            if let Some(fields) = state.as_object_mut() {
                fields.remove("rollout"); // promoted: no rollout in flight
            }
            let steps = plan.steps.len();
            state["conditions"] = json!([condition(
                "Ready",
                true,
                "Promoted",
                &format!("promoted {reference} (canary {steps} steps)"),
                &ts
            )]);
            write_status(client, key, sp, state).await.map(|_| ())
        }
    }
}

/// Abort the in-flight canary: delete its children, weight the split back to
/// the stable backend; the stable image is never touched.
pub(super) async fn abort(
    client: &Arc<dyn Client>,
    key: &Key,
    sp: &Value,
    ns: &str,
    pipeline: &str,
    uid: &str,
) -> Result<(), ControllerError> {
    let Some(release) = plan::parse_release(sp) else {
        return fail(
            client,
            key,
            sp,
            "ReleaseInvalid",
            "spec.release is not a PipelineRelease",
            None,
        )
        .await;
    };
    let plan = match plan::validate_names(sp, &release) {
        Ok(plan) => plan,
        Err(message) => return fail(client, key, sp, "ReleaseInvalid", &message, None).await,
    };
    tracing::debug!(pipeline, uid, namespace = ns, "canary: abort requested");
    // Deployment first: stop the canary pods before its Service disappears.
    apply::delete_if_present(
        client,
        &Key::new("apps", "deployments", ns, &plan.canary_deployment),
    )
    .await?;
    apply::delete_if_present(client, &Key::new("", "services", ns, &plan.canary_service)).await?;
    let split_key = apply::split_key(ns, &plan.traffic_split);
    apply::patch_split(client, &split_key, plan::desired_backends(&plan, 0)).await?;
    let mut state = status_of(sp);
    if state.get("phase").and_then(Value::as_str) == Some(api::pipeline::PHASE_ABORTED)
        && state.get("rollout").is_none()
    {
        return Ok(()); // already converged: no status churn on every resync
    }
    let ts = now();
    state["phase"] = json!(api::pipeline::PHASE_ABORTED);
    if let Some(fields) = state.as_object_mut() {
        fields.remove("rollout"); // nothing in flight after an abort
    }
    state["conditions"] = json!([condition(
        "Ready",
        false,
        "Aborted",
        "aborted canary release; the stable image was not touched",
        &ts
    )]);
    write_status(client, key, sp, state).await.map(|_| ())
}

/// `status.rollout` as a typed object (`None` when absent or malformed).
fn rollout_of(sp: &Value) -> Option<api::pipeline::RolloutStatus> {
    sp.pointer("/status/rollout")
        .and_then(|raw| serde_json::from_value(raw.clone()).ok())
}

/// Wall clock in Unix seconds (the soak clock of `step_decision`).
fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// One-shot `Failed` status write (`Ready=False` with `reason`).
///
/// Anti-churn (T8.5 bug fix): the pipeline worker resyncs every 2s and
/// `now()` is a fresh timestamp on every pass, so an unconditional write
/// minted ~43k revisions/day/pipeline. The write is therefore conditional --
/// [`failure_recorded`] says the object already carries this exact failure
/// (phase, reason, message and, for a `roll`, the observed tag) and the
/// pass returns `Ok` without touching storage.
///
/// `tag` (`Some` from [`roll`], `None` from [`abort`]) advances
/// `status.observedTag` so a failed release is HELD: the reconciler's
/// `observedTag == spec.image.tag` short-circuit stops re-entering the
/// release. An abort never touches it (Q58).
async fn fail(
    client: &Arc<dyn Client>,
    key: &Key,
    sp: &Value,
    reason: &str,
    message: &str,
    tag: Option<&str>,
) -> Result<(), ControllerError> {
    if failure_recorded(sp, reason, message, tag) {
        return Ok(()); // already reported: no status churn on the resync
    }
    let mut state = status_of(sp);
    state["phase"] = json!(api::pipeline::PHASE_FAILED);
    if let Some(tag) = tag {
        state["observedTag"] = json!(tag);
    }
    state["conditions"] = json!([condition("Ready", false, reason, message, &now())]);
    write_status(client, key, sp, state).await.map(|_| ())
}

/// True when `sp` already reports exactly this failure, i.e. the pass has
/// nothing new to write. The condition timestamp is deliberately ignored
/// (it is the only field that would differ on a plain resync).
fn failure_recorded(sp: &Value, reason: &str, message: &str, tag: Option<&str>) -> bool {
    let state = status_of(sp);
    if state.get("phase").and_then(Value::as_str) != Some(api::pipeline::PHASE_FAILED) {
        return false;
    }
    if let Some(tag) = tag {
        if state.get("observedTag").and_then(Value::as_str) != Some(tag) {
            return false; // a different tag failed since: re-report
        }
    }
    let Some(ready) = state
        .pointer("/conditions")
        .and_then(Value::as_array)
        .and_then(|conditions| conditions.first())
    else {
        return false;
    };
    let field = |key: &str| ready.get(key).and_then(Value::as_str);
    field("type") == Some("Ready")
        && field("status") == Some("False")
        && field("reason") == Some(reason)
        && field("message") == Some(message)
}

/// `"<kind> <ns>/<name> is missing"` for the failure messages.
fn missing(kind: &str, ns: &str, name: &str) -> String {
    format!("{kind} {ns}/{name} does not exist")
}

/// `"<kind> <ns>/<name> is not owned by this pipeline"` (adoption refusal).
fn conflict(kind: &str, ns: &str, name: &str) -> String {
    format!("{kind} {ns}/{name} exists and is not owned by this pipeline")
}
