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

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::{json, Value};
use storage::Key;

use api::build::{PHASE_BUILDING, PHASE_FAILED, PHASE_PENDING, PHASE_PUBLISHING, PHASE_SUCCEEDED};

use crate::client::Client;
use crate::error::ControllerError;
use crate::object::{name, namespace};

use super::executor;
use super::plan::{blank, build_args, join_logs, phase_of, str_at, timeout_for};
use super::publish;
use super::status::{bounded_log, condition, now, status_of, write_status};
use super::PipelineConfig;

const API_GROUP: &str = "init-pro.io";

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
            finalize(client, key, &current, &outcome).await
        }
        Err(e) => {
            let message = format!("push to {addr}/{repo}:{tag} failed: {e}");
            let outcome = Outcome::failed(reference, message, &log);
            finalize(client, key, &current, &outcome).await
        }
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
