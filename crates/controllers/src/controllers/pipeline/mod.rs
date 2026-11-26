//! `ServicePipeline` controller (TODO **T8.3**, decision **Q56**): the M2
//! CI/CD pipeline face.
//!
//! One ServicePipeline object = one service = one image, built and published
//! once per tag transition:
//!
//! * [`reconcile`] compares `spec.image.tag` with `status.observedTag`.
//!   A non-terminal child `Build` holds the pipeline in `Building` /
//!   `Publishing`; a terminal one is promoted (target Deployment image
//!   patched, `observedTag` advanced, `Ready=True`) or reported failed
//!   (`Ready=False`, reason `BuildFailed`). A tag with no build history
//!   creates exactly ONE child named
//!   `<pipeline>-<tag>-<random>`; a `Failed` build for the SAME tag is held
//!   (no rebuild loop) -- re-applying a new tag is the retry.
//! * [`build::reconcile`] runs the per-Build phase machine
//!   `Building -> Publishing -> Succeeded|Failed`, driving the docker CLI
//!   through [`executor`] and the embedded registry through [`publish`].
//! * With `spec.release` set (T8.5/Q58) a Succeeded build no longer patches
//!   the stable Deployment: [`canary::roll`] owns a canary Deployment/Service
//!   pair plus the `TrafficSplit` weights and promotes on the last soaked
//!   step; `spec.release.abort` runs [`canary::abort`] before anything else.
//!
//! Disabled posture (Q56): with `ManagerConfig::pipeline == None` the runner
//! spawns no worker and enqueues nothing at all; [`reconcile`] is then a
//! no-op, so an install that never sets `--pipeline-*` behaves exactly like
//! the pre-T8.3 controller set.
//!
//! JSON-only wire (Q10): every function works on raw `serde_json::Value`.
//! Status writes are full-object CAS (no `/status` subresource) via
//! [`status::write_status`], which also owns the bounded log tail.

pub mod build;
/// Canary release controller (TODO **T8.5**, decision **Q58**): owns the
/// `<deployment>-canary` / `<service>-canary` children and the per-step
/// `TrafficSplit` weights; promotes into the stable Deployment or aborts.
mod canary;
pub mod executor;
/// Pure decisions over raw JSON (naming, ownership, phases, image patch).
mod plan;
pub mod publish;
mod status;
#[cfg(test)]
mod tests;

use std::sync::Arc;

use serde_json::{json, Value};
use storage::{Key, KeyPrefix};

use crate::client::Client;
use crate::error::ControllerError;
use crate::id::add_label;
use crate::object::{name, namespace, owner_reference, resource_version, semantic_eq};
use plan::{
    active_phase, build_name_with, build_ref, is_terminal, latest_build, owned_build, phase_of,
    set_container_image, str_at, string_array,
};
use status::{condition, now, status_of, write_status};

/// API group shared by `ServicePipeline` + `Build` (same as `api`'s).
const API_GROUP: &str = "init-pro.io";
/// `ServicePipeline` kind (the controller ownerReference kind).
const PIPELINE_KIND: &str = "ServicePipeline";
/// Label stamped on Build children: owning pipeline name.
const PIPELINE_LABEL: &str = "init-pro.io/pipeline";

/// Pipeline controller configuration (Q56). `None` in
/// `ManagerConfig::pipeline` keeps the whole pipeline plane off.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PipelineConfig {
    /// Registry authority pushes target (`host:port`); a repository that
    /// carries its own host overrides it per image.
    pub registry_addr: String,
    /// Scratch root for `docker save` tarballs (`<data_dir>/pipeline`).
    pub scratch_dir: std::path::PathBuf,
    /// Docker CLI to exec (env `INIT_PRO_DOCKER`, default `docker`).
    pub docker_bin: String,
    /// Build/save timeout when `spec.timeoutSeconds == 0` (default 900).
    pub default_timeout_seconds: u64,
    /// A1 hardening: upper bound on the *decoded* `docker save` archive a
    /// Build pass imports (default 8 GiB). The importer streams layers to
    /// `<scratch>/import` so the bound guards the DISK, never the RSS.
    pub max_image_bytes: u64,
}

/// Reconcile one `ServicePipeline` (`sp`, raw JSON).
pub async fn reconcile(
    client: &Arc<dyn Client>,
    sp: &Value,
    cfg: &Option<PipelineConfig>,
) -> Result<(), ControllerError> {
    if cfg.is_none() {
        return Ok(()); // pipeline plane disabled (no worker either)
    }
    let (Some(ns), Some(pipeline)) = (namespace(sp), name(sp)) else {
        return Ok(());
    };
    let repository = str_at(sp, "/spec/image/repository").trim();
    let tag = str_at(sp, "/spec/image/tag").trim();
    if repository.is_empty() || tag.is_empty() {
        return Ok(()); // no image configured yet
    }
    if sp.pointer("/spec/paused").and_then(Value::as_bool) == Some(true) {
        return Ok(()); // paused: hold the observed tag
    }
    let context = str_at(sp, "/spec/source/context").trim();
    let archive = str_at(sp, "/spec/source/archive").trim();
    if context.is_empty() && archive.is_empty() {
        return Ok(()); // no source: nothing to build
    }
    // Q58 / namespace DRAIN: a pipeline being deleted must not create
    // children that the namespace drain would then race (its canary
    // Deployment/Service are reaped by the GC cascade anyway).
    if sp
        .pointer("/metadata/deletionTimestamp")
        .map(|v| !v.is_null())
        .unwrap_or(false)
    {
        return Ok(());
    }
    let reference = format!("{}:{}", repository.trim_end_matches(':'), tag);
    let uid = sp
        .pointer("/metadata/uid")
        .and_then(Value::as_str)
        .unwrap_or("");
    let key = Key::new(API_GROUP, "servicepipelines", ns, pipeline);

    // Q58 abort: delete the canary children and reset the split. Runs before
    // the build path so an abort never starts or promotes a build.
    if canary::abort_requested(sp) {
        return canary::abort(client, &key, sp, ns, pipeline, uid).await;
    }

    // Children are listed FRESH through the client (the cache lags and this
    // pass owns its CAS base); only controller-owned children of THIS
    // pipeline count.
    let builds = client
        .list(&KeyPrefix::new(API_GROUP, "builds", Some(ns.to_string())))
        .await?;
    let owned: Vec<&Value> = builds
        .iter()
        .filter(|b| owned_build(b, pipeline, uid))
        .collect();

    // In-flight child: mirror its phase and let its worker finish (no
    // duplicate build, no promotion before the push lands).
    if let Some(active) = owned.iter().find(|b| !is_terminal(b)) {
        let mut state = status_of(sp);
        state["phase"] = json!(active_phase(active));
        return write_status(client, &key, sp, state).await.map(|_| ());
    }

    // Already promoted for this tag: never regress, never rebuild.
    if status_of(sp).get("observedTag").and_then(Value::as_str) == Some(tag) {
        return Ok(());
    }

    if let Some(latest) = latest_build(&owned) {
        if str_at(latest, "/spec/image/tag").trim() == tag {
            if phase_of(latest) == api::build::PHASE_FAILED {
                return hold_failure(client, &key, sp, latest, tag).await;
            }
            if phase_of(latest) == api::build::PHASE_SUCCEEDED {
                // Q58: a canary release never patches the stable Deployment
                // directly -- it rolls the canary and promotes on the last
                // soaked step.
                if canary::has_release(sp) {
                    return canary::roll(
                        client, &key, sp, latest, ns, pipeline, uid, tag, &reference,
                    )
                    .await;
                }
                return promote(client, &key, sp, latest, ns, tag, &reference).await;
            }
        }
    }

    create_build(client, &key, sp, ns, pipeline, uid, tag, &reference).await
}

/// Latest build for this tag already Failed: report once, never rebuild.
async fn hold_failure(
    client: &Arc<dyn Client>,
    key: &Key,
    sp: &Value,
    latest: &Value,
    tag: &str,
) -> Result<(), ControllerError> {
    let latest_name = name(latest).unwrap_or_default();
    let mut state = status_of(sp);
    if state.pointer("/lastBuild/name").and_then(Value::as_str) == Some(latest_name) {
        return Ok(()); // already reported: hold Failed (no rebuild loop)
    }
    let ts = now();
    state["phase"] = json!(api::pipeline::PHASE_FAILED);
    state["lastBuild"] = build_ref(latest);
    state["conditions"] = json!([condition(
        "Ready",
        false,
        "BuildFailed",
        &format!("build {latest_name} failed for tag {tag}"),
        &ts
    )]);
    write_status(client, key, sp, state).await.map(|_| ())
}

/// Latest build for this tag Succeeded: patch the target Deployment (Q56)
/// and record the promotion.
async fn promote(
    client: &Arc<dyn Client>,
    key: &Key,
    sp: &Value,
    latest: &Value,
    ns: &str,
    tag: &str,
    reference: &str,
) -> Result<(), ControllerError> {
    // Best-effort image patch: a missing Deployment/container never fails
    // the reconcile (the image is published either way); a CAS race does.
    patch_target_deployment(client, ns, sp, reference).await?;
    let ts = now();
    let mut state = status_of(sp);
    state["phase"] = json!(api::pipeline::PHASE_PROMOTED);
    state["observedTag"] = json!(tag);
    state["lastBuild"] = build_ref(latest);
    state["conditions"] = json!([condition(
        "Ready",
        true,
        "Promoted",
        &format!("promoted {reference}"),
        &ts
    )]);
    write_status(client, key, sp, state).await.map(|_| ())
}

/// Create the ONE `Build` child for `(pipeline, tag)`, then mark Building.
#[allow(clippy::too_many_arguments)] // the pipeline identity is the call site's
async fn create_build(
    client: &Arc<dyn Client>,
    key: &Key,
    sp: &Value,
    ns: &str,
    pipeline: &str,
    uid: &str,
    tag: &str,
    reference: &str,
) -> Result<(), ControllerError> {
    let build_name = build_name_with(pipeline, tag, &crate::id::rand_suffix(5));
    let spec = api::build::BuildSpec {
        pipeline: pipeline.to_string(),
        image: api::pipeline::PipelineImage {
            repository: str_at(sp, "/spec/image/repository").trim().to_string(),
            tag: tag.to_string(),
        },
        source: api::pipeline::PipelineSource {
            context: str_at(sp, "/spec/source/context").trim().to_string(),
            dockerfile: str_at(sp, "/spec/source/dockerfile").trim().to_string(),
            archive: str_at(sp, "/spec/source/archive").trim().to_string(),
        },
        args: string_array(sp.pointer("/spec/build/args")),
        timeout_seconds: sp
            .pointer("/spec/build/timeoutSeconds")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            .min(u32::MAX as u64) as u32,
    };
    let mut value = serde_json::to_value(api::build::new_build(&build_name, ns, spec))
        .map_err(|e| ControllerError::Internal(format!("encode Build {ns}/{build_name}: {e}")))?;
    value["metadata"]["ownerReferences"] = json!([owner_reference(
        pipeline,
        uid,
        PIPELINE_KIND,
        api::pipeline::API_VERSION
    )]);
    add_label(&mut value, PIPELINE_LABEL, pipeline);
    let build_key = Key::new(API_GROUP, "builds", ns, &build_name);
    match client.create(&build_key, value).await {
        Ok(_) => tracing::debug!(build = %build_name, namespace = ns, "pipeline created Build"),
        Err(e) if e.is_already_exists() => {} // benign: next pass observes it
        Err(e) => return Err(e),
    }
    let mut state = status_of(sp);
    state["phase"] = json!(api::pipeline::PHASE_BUILDING);
    state["lastBuild"] = json!({"name": build_name, "image": reference});
    write_status(client, key, sp, state).await.map(|_| ())
}

/// Patch the target Deployment's container image (Q56 promotion step; Q58
/// promote step of a canary release).
pub(super) async fn patch_target_deployment(
    client: &Arc<dyn Client>,
    ns: &str,
    sp: &Value,
    reference: &str,
) -> Result<(), ControllerError> {
    let deployment = str_at(sp, "/spec/target/deployment").trim();
    if deployment.is_empty() {
        return Ok(()); // no target: publish only
    }
    let key = Key::new("apps", "deployments", ns, deployment);
    let Some(dep) = client.get(&key).await? else {
        tracing::warn!(
            deployment,
            namespace = ns,
            "pipeline target Deployment is missing; image patch skipped"
        );
        return Ok(());
    };
    let mut next = dep.clone();
    if !set_container_image(
        &mut next,
        str_at(sp, "/spec/target/container").trim(),
        reference,
    ) {
        tracing::warn!(
            deployment,
            namespace = ns,
            "pipeline target container not found; image patch skipped"
        );
        return Ok(());
    }
    if semantic_eq(&dep, &next) {
        return Ok(()); // already on this image
    }
    client
        .update(&key, next, resource_version(&dep))
        .await
        .map(|_| ())
        .map_err(|e| {
            if e.is_conflict() {
                ControllerError::Conflict(format!(
                    "pipeline image patch raced on Deployment {ns}/{deployment}"
                ))
            } else {
                e
            }
        })
}
