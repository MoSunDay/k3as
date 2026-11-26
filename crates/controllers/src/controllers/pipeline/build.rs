//! `Build` phase machine (TODO **T8.3**, decision **Q56**).
//!
//! One `Build` child = one `docker build` (+ `docker save`) + one registry
//! push, driven from `Pending`/`Building` to a terminal phase:
//!
//! ```text
//! Pending|Building --(build ok)--> Publishing --(push ok)--> Succeeded
//!         |                            |
//!         +--(failed / timeout)-------+--> Failed
//! ```
//!
//! Source modes (Q56): a non-empty `spec.source.context` runs `docker build`
//! then `docker save`; `spec.source.archive` is an already-built
//! `docker save` tarball and skips the docker CLI entirely (the docker-free
//! CI/e2e path). Both feed [`registry::push_image`] after
//! [`super::publish::registry_and_repo`] splits the repository from the
//! registry authority.
//!
//! Status writes are full-object CAS on the object this pass re-read
//! (conflicts propagate and the worker requeues); log tails are bounded by
//! [`super::status::bounded_log`]. A terminal build is never re-run -- the
//! spec is immutable, a retry is a new tag.
//!
//! Host image hygiene (T8.3 bug fix, B8): every Build used to leave its
//! image on the host docker daemon, so node disk only grew. Builds are now
//! labeled with the owning pipeline ([`plan::IMAGE_LABEL`]), the pipeline's
//! older images are pruned back to [`plan::BUILD_IMAGE_RETENTION`] before a
//! build, and the just-pushed copy is dropped once the registry owns it
//! (unless it is the promoted image). Every hygiene call is best effort:
//! failures are logged and never fail the Build.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use storage::Key;

use api::build::{PHASE_BUILDING, PHASE_FAILED, PHASE_PENDING, PHASE_PUBLISHING, PHASE_SUCCEEDED};

use crate::client::Client;
use crate::error::ControllerError;
use crate::object::{name, namespace};

use super::executor;
use super::plan::{
    blank, build_args, image_label_value, is_promoted, join_logs, list_images_args,
    next_push_retry, phase_of, prune_targets, rmi_args, str_at, timeout_for, BUILD_IMAGE_RETENTION,
    MAX_PUSH_RETRIES,
};
use super::publish;
use super::status::{bounded_log, condition, now, status_of, write_status};
use super::PipelineConfig;

const API_GROUP: &str = "init-pro.io";

/// Budget for the best-effort `docker images` / `docker rmi` hygiene calls:
/// short on purpose, a wedged daemon must not stretch a Build pass.
const HYGIENE_TIMEOUT: Duration = Duration::from_secs(30);

/// Reconcile one `Build` (raw JSON) against its phase machine.
pub async fn reconcile(
    client: &Arc<dyn Client>,
    build: &Value,
    cfg: &PipelineConfig,
) -> Result<(), ControllerError> {
    let (Some(ns), Some(build_name)) = (namespace(build), name(build)) else {
        return Ok(()); // malformed object: nothing to run
    };
    let key = Key::new(API_GROUP, "builds", ns, build_name);
    let reference = format!(
        "{}:{}",
        str_at(build, "/spec/image/repository").trim_end_matches(':'),
        str_at(build, "/spec/image/tag")
    );
    let phase = phase_of(build);
    if phase == PHASE_SUCCEEDED || phase == PHASE_FAILED {
        return Ok(()); // terminal: never re-run (immutable spec, Q56)
    }
    if phase == PHASE_PUBLISHING {
        let current = build.clone();
        return publish_phase(client, &key, current, cfg, ns, build_name, &reference, "").await;
    }
    if !(phase.is_empty() || phase == PHASE_PENDING || phase == PHASE_BUILDING) {
        tracing::warn!(
            build = build_name,
            namespace = ns,
            phase,
            "unknown Build phase; leaving untouched"
        );
        return Ok(());
    }
    match build_phase(client, &key, build, cfg, &reference).await? {
        Some((current, log)) => {
            publish_phase(client, &key, current, cfg, ns, build_name, &reference, &log).await
        }
        None => Ok(()), // build failed: the terminal status is already written
    }
}

/// `Pending`/`Building` step: stamp status, run `docker build`, advance to
/// `Publishing` in the same pass. `None` = the build failed terminally.
async fn build_phase(
    client: &Arc<dyn Client>,
    key: &Key,
    build: &Value,
    cfg: &PipelineConfig,
    reference: &str,
) -> Result<Option<(Value, String)>, ControllerError> {
    let context = str_at(build, "/spec/source/context").trim().to_string();
    let archive = str_at(build, "/spec/source/archive").trim().to_string();
    if context.is_empty() && archive.is_empty() {
        let outcome = Outcome::failed(reference, "no source configured (context or archive)", "");
        finalize(client, key, build, &outcome).await?;
        return Ok(None);
    }
    // Record Building FIRST: a long docker build must be observable, and the
    // pre-existing startedAt is preserved so the write stays idempotent.
    let mut state = status_of(build);
    if blank(state.get("startedAt")) {
        state["startedAt"] = json!(now());
    }
    state["phase"] = json!(PHASE_BUILDING);
    state["image"] = json!(reference);
    let current = write_status(client, key, build, state).await?;

    let log = if context.is_empty() {
        // Archive-only (Q56 docker-free path): nothing to build locally.
        String::new()
    } else {
        // B8: keep the host's copy of this pipeline bounded BEFORE adding
        // one more image (best effort, never fatal).
        prune_before_build(cfg, &current).await;
        let args = build_args(&current, reference);
        let budget = timeout_for(&current, cfg.default_timeout_seconds);
        let out = executor::run(&cfg.docker_bin, &args, budget).await;
        if !out.success {
            let message = if out.timed_out {
                "docker build timed out".to_string()
            } else {
                format!("docker build failed (exit {:?})", out.code)
            };
            let outcome = Outcome::failed(reference, message, &out.log);
            finalize(client, key, &current, &outcome).await?;
            return Ok(None);
        }
        out.log
    };

    // Advance to Publishing; keep startedAt/image and the build log so far.
    let mut state = status_of(&current);
    state["phase"] = json!(PHASE_PUBLISHING);
    state["image"] = json!(reference);
    if !log.is_empty() {
        state["logTail"] = json!(bounded_log(&log));
    }
    let current = write_status(client, key, &current, state).await?;
    Ok(Some((current, log)))
}

/// `Publishing` step: obtain the `docker save` payload, push it, finalize.
#[allow(clippy::too_many_arguments)] // 8: the dispatch identity + carried log
async fn publish_phase(
    client: &Arc<dyn Client>,
    key: &Key,
    current: Value,
    cfg: &PipelineConfig,
    ns: &str,
    build_name: &str,
    reference: &str,
    prior_log: &str,
) -> Result<(), ControllerError> {
    let archive = str_at(&current, "/spec/source/archive").trim().to_string();
    let repository = str_at(&current, "/spec/image/repository")
        .trim()
        .to_string();
    let tag = str_at(&current, "/spec/image/tag").trim().to_string();
    let (addr, repo) = publish::registry_and_repo(&repository, &cfg.registry_addr);
    if repo.is_empty() || tag.is_empty() {
        let outcome = Outcome::failed(reference, "image repository/tag is empty", prior_log);
        return finalize(client, key, &current, &outcome).await;
    }

    let mut log = prior_log.to_string();
    let save = if !archive.is_empty() {
        match parse_save(PathBuf::from(&archive), cfg).await {
            Ok(save) => save,
            Err(e) => {
                let outcome = Outcome::failed(reference, format!("docker save archive: {e}"), &log);
                return finalize(client, key, &current, &outcome).await;
            }
        }
    } else {
        // Context mode: `docker save` the image the build just produced.
        let dir = cfg.scratch_dir.join(ns).join(build_name);
        if let Err(e) = tokio::fs::create_dir_all(&dir).await {
            // Infrastructure, not the image: retry instead of burning the tag.
            return Err(ControllerError::Internal(format!(
                "pipeline scratch dir {}: {e}",
                dir.display()
            )));
        }
        let tar = dir.join("image.tar");
        let args = vec![
            "save".to_string(),
            "-o".to_string(),
            tar.display().to_string(),
            reference.to_string(),
        ];
        let budget = timeout_for(&current, cfg.default_timeout_seconds);
        let out = executor::run(&cfg.docker_bin, &args, budget).await;
        log = join_logs(&log, &out.log);
        if !out.success {
            let _ = tokio::fs::remove_dir_all(&dir).await;
            let message = if out.timed_out {
                "docker save timed out".to_string()
            } else {
                format!("docker save failed (exit {:?})", out.code)
            };
            let outcome = Outcome::failed(reference, message, &log);
            return finalize(client, key, &current, &outcome).await;
        }
        let parsed = parse_save(tar, cfg).await;
        let _ = tokio::fs::remove_dir_all(&dir).await; // best effort
        match parsed {
            Ok(save) => save,
            Err(e) => {
                let outcome = Outcome::failed(reference, format!("docker save archive: {e}"), &log);
                return finalize(client, key, &current, &outcome).await;
            }
        }
    };

    let push = registry::to_push_image(&save);
    match registry::push_image(&addr, &repo, &tag, &push).await {
        Ok(digest) => {
            let message = format!("published {addr}/{repo}:{tag}");
            let outcome = Outcome::succeeded(reference, &digest, message, &log);
            let finalized = finalize(client, key, &current, &outcome).await;
            // B8: the registry owns the image now -- drop the host copy.
            prune_after_push(client, cfg, &current, reference).await;
            finalized
        }
        Err(e) => {
            // B10: a transport failure (connect/timeout/framing) is not a
            // verdict on the image -- the embedded registry may be briefly
            // unavailable (restart, drained listener). Count it in
            // `status.pushRetries` and return Internal so the worker
            // rate-limited-requeues the key; the next pass re-enters
            // Publishing. Protocol/invalid answers stay terminal: a retry
            // cannot change them. The counter survives in status, so the
            // cap holds across controller restarts too.
            let retry = matches!(e, registry::PushError::Io(_))
                .then(|| next_push_retry(&status_of(&current)))
                .flatten();
            if let Some(n) = retry {
                let line = format!("push attempt {n}/{MAX_PUSH_RETRIES} failed: {e}");
                let mut state = status_of(&current);
                state["pushRetries"] = json!(n);
                state["logTail"] = json!(bounded_log(&join_logs(
                    str_at(&current, "/status/logTail"),
                    &line
                )));
                write_status(client, key, &current, state).await?;
                return Err(ControllerError::Internal(format!(
                    "pipeline push transport retry {n}/{MAX_PUSH_RETRIES}: {e}"
                )));
            }
            let seen = status_of(&current)
                .get("pushRetries")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let message = if seen == 0 {
                format!("push to {addr}/{repo}:{tag} failed: {e}")
            } else {
                format!("push to {addr}/{repo}:{tag} failed after {seen} transport retries: {e}")
            };
            let outcome = Outcome::failed(reference, message, &log);
            finalize(client, key, &current, &outcome).await
        }
    }
}

/// B8 retention pass: prune the pipeline's images down to
/// [`BUILD_IMAGE_RETENTION`] (newest kept) before a new `docker build`.
async fn prune_before_build(cfg: &PipelineConfig, build: &Value) {
    let Some(label) = image_label_value(build) else {
        return; // no pipeline identity: nothing to select images by
    };
    let args = list_images_args(&label);
    let out = executor::run(&cfg.docker_bin, &args, HYGIENE_TIMEOUT).await;
    if !out.success {
        tracing::warn!(
            pipeline = %label,
            "pipeline image retention: `docker images` failed; keeping the host images"
        );
        return;
    }
    for handle in prune_targets(&out.log, BUILD_IMAGE_RETENTION) {
        rmi(cfg, &handle).await;
    }
}

/// B8 post-push pass: drop the just-built image from the host docker unless
/// it is the pipeline's currently-promoted image (that copy stays warm).
async fn prune_after_push(
    client: &Arc<dyn Client>,
    cfg: &PipelineConfig,
    build: &Value,
    reference: &str,
) {
    // An archive-sourced Build never touched the host daemon (Q56
    // docker-free path): there is no local copy to remove.
    if !str_at(build, "/spec/source/archive").trim().is_empty() {
        return;
    }
    if is_promoted(reference, &promoted_image(client, build).await) {
        tracing::debug!(
            image = reference,
            "pipeline image kept: it is the promoted one"
        );
        return;
    }
    rmi(cfg, reference).await;
}

/// `status.lastBuild.image` of the owning ServicePipeline ("" when it is
/// gone, unknown, or has promoted nothing yet).
async fn promoted_image(client: &Arc<dyn Client>, build: &Value) -> String {
    let ns = namespace(build).unwrap_or_default();
    let pipeline = str_at(build, "/spec/pipeline").trim();
    if ns.is_empty() || pipeline.is_empty() {
        return String::new();
    }
    let key = Key::new(API_GROUP, "servicepipelines", ns, pipeline);
    match client.get(&key).await {
        Ok(Some(sp)) => str_at(&sp, "/status/lastBuild/image").trim().to_string(),
        Ok(None) => String::new(),
        Err(e) => {
            tracing::warn!(error = %e, "pipeline image retention: pipeline read failed");
            String::new()
        }
    }
}

/// Best-effort `docker rmi <handle>`: a failure is a warning, never a Build
/// failure (an in-use or multiply-tagged image is simply kept).
async fn rmi(cfg: &PipelineConfig, handle: &str) {
    let args = rmi_args(handle);
    let out = executor::run(&cfg.docker_bin, &args, HYGIENE_TIMEOUT).await;
    if out.success {
        tracing::debug!(image = handle, "pipeline image pruned from the host docker");
    } else {
        tracing::warn!(
            image = handle,
            "pipeline image retention: `docker rmi` failed; the host image is kept"
        );
    }
}

/// Terminal transition payload.
struct Outcome<'a> {
    phase: &'a str,
    reference: &'a str,
    digest: &'a str,
    message: String,
    log: &'a str,
}

impl<'a> Outcome<'a> {
    fn failed(reference: &'a str, message: impl Into<String>, log: &'a str) -> Self {
        Outcome {
            phase: PHASE_FAILED,
            reference,
            digest: "",
            message: message.into(),
            log,
        }
    }

    fn succeeded(reference: &'a str, digest: &'a str, message: String, log: &'a str) -> Self {
        Outcome {
            phase: PHASE_SUCCEEDED,
            reference,
            digest,
            message,
            log,
        }
    }
}

/// CAS-write the terminal status (phase, digest, timestamps, bounded log,
/// `Completed` condition).
async fn finalize(
    client: &Arc<dyn Client>,
    key: &Key,
    current: &Value,
    outcome: &Outcome<'_>,
) -> Result<(), ControllerError> {
    let ts = now();
    let ok = outcome.phase == PHASE_SUCCEEDED;
    let mut state = status_of(current);
    state["phase"] = json!(outcome.phase);
    if !outcome.reference.is_empty() {
        state["image"] = json!(outcome.reference);
    }
    if !outcome.digest.is_empty() {
        state["digest"] = json!(outcome.digest);
    }
    if blank(state.get("startedAt")) {
        state["startedAt"] = json!(ts);
    }
    state["finishedAt"] = json!(ts);
    if !outcome.log.is_empty() {
        state["logTail"] = json!(bounded_log(outcome.log));
    }
    state["conditions"] = json!([condition(
        "Completed",
        ok,
        if ok { "Published" } else { "BuildFailed" },
        &outcome.message,
        &ts
    )]);
    write_status(client, key, current, state).await.map(|_| ())
}

/// Parse a `docker save` archive off the async runtime (blocking file IO).
///
/// A1: layers are staged under `<scratch>/import` (never inside the
/// per-build scratch dir, which the caller deletes right after this call)
/// and streamed into the registry, so a multi-GiB image never enters RAM.
async fn parse_save(path: PathBuf, cfg: &PipelineConfig) -> Result<registry::DockerSave, String> {
    let staging_root = cfg.scratch_dir.join("import");
    let limits = registry::ImportLimits {
        max_image_bytes: cfg.max_image_bytes,
        max_metadata_bytes: registry::DEFAULT_MAX_METADATA_BYTES,
    };
    match tokio::task::spawn_blocking(move || {
        registry::parse_docker_save_file_with(&path, &staging_root, &limits)
    })
    .await
    {
        Ok(result) => result,
        Err(e) => Err(format!("docker save parse task failed: {e}")),
    }
}
