//! Pure pipeline decisions over raw JSON (TODO **T8.3**, decision **Q56**).
//!
//! Everything here is side-effect free so the controller's branching is
//! unit-testable without storage, docker, or a registry: build naming
//! (DNS-1123 label safe), child ownership, newest-build ordering, phase
//! mapping, the Deployment container-image selection, the
//! `status.lastBuild` projection and the host-image hygiene argv/retention
//! selection (the `init-pro.pipeline` label + `docker rmi` pruning).
//!
//! The reconcilers in [`super::reconcile`] / [`super::build::reconcile`]
//! only sequence these decisions around storage + CLI/registry effects.

use std::time::Duration;

use serde_json::{json, Value};

use crate::object::{controller_of, name, namespace};

use super::PIPELINE_KIND;

/// `status.lastBuild` summary of a terminal Build.
pub(super) fn build_ref(build: &Value) -> Value {
    let image = str_at(build, "/status/image");
    let image = if image.is_empty() {
        format!(
            "{}:{}",
            str_at(build, "/spec/image/repository").trim_end_matches(':'),
            str_at(build, "/spec/image/tag")
        )
    } else {
        image.to_string()
    };
    json!({
        "name": name(build).unwrap_or_default(),
        "image": image,
        "digest": str_at(build, "/status/digest"),
        "startedAt": str_at(build, "/status/startedAt"),
        "finishedAt": str_at(build, "/status/finishedAt"),
    })
}

/// `"/a/b"` string value (empty when absent or not a string).
pub(super) fn str_at<'a>(value: &'a Value, path: &str) -> &'a str {
    value.pointer(path).and_then(Value::as_str).unwrap_or("")
}

/// B10 (long-run bug sweep): cap on transport-level registry push retries
/// before a `Build` is finalized `Failed`.
pub(super) const MAX_PUSH_RETRIES: u64 = 5;

/// `status.pushRetries` after one more transport failure (B10): `None`
/// once the cap is reached, `Some(n)` with the incremented counter
/// otherwise. A missing/non-numeric counter counts as zero.
pub(super) fn next_push_retry(status: &Value) -> Option<u64> {
    let seen = status
        .get("pushRetries")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    (seen < MAX_PUSH_RETRIES).then_some(seen + 1)
}

/// String array at `value` (non-string entries dropped).
pub(super) fn string_array(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// `Build.status.phase` (empty when unset).
pub(super) fn phase_of(build: &Value) -> &str {
    str_at(build, "/status/phase")
}

/// Terminal build phase (`Succeeded` / `Failed` builds are never re-run).
pub(super) fn is_terminal(build: &Value) -> bool {
    let phase = phase_of(build);
    phase == api::build::PHASE_SUCCEEDED || phase == api::build::PHASE_FAILED
}

/// Pipeline phase mirrored from an in-flight build.
pub(super) fn active_phase(build: &Value) -> &'static str {
    if phase_of(build) == api::build::PHASE_PUBLISHING {
        api::pipeline::PHASE_PUBLISHING
    } else {
        api::pipeline::PHASE_BUILDING
    }
}

/// Controller-owned child of `(pipeline, uid)` (uid compared when both set).
pub(super) fn owned_build(build: &Value, pipeline: &str, uid: &str) -> bool {
    let Some(owner) = controller_of(build) else {
        return false;
    };
    if owner.get("kind").and_then(Value::as_str) != Some(PIPELINE_KIND) {
        return false;
    }
    if owner.get("name").and_then(Value::as_str) != Some(pipeline) {
        return false;
    }
    let owner_uid = owner.get("uid").and_then(Value::as_str).unwrap_or("");
    owner_uid.is_empty() || uid.is_empty() || owner_uid == uid
}

/// Newest owned build: max by `metadata.creationTimestamp`, then name.
pub(super) fn latest_build<'a>(builds: &[&'a Value]) -> Option<&'a Value> {
    builds
        .iter()
        .copied()
        .max_by(|a, b| sort_key(a).cmp(&sort_key(b)))
}

/// Newest owned build whose `spec.image.tag` is `tag`. Filtering by tag
/// FIRST is load-bearing: the apiserver stamps no creationTimestamp, so the
/// name is the effective order key and `p-b9-x` sorts above `p-b10-x` -- a
/// plain "newest child" would never match the desired tag after the 9->10
/// rollover and storm one Build per resync (scripts/soak-e2e.sh, batch F).
pub(super) fn latest_build_for_tag<'a>(builds: &[&'a Value], tag: &str) -> Option<&'a Value> {
    let same_tag: Vec<&Value> = builds
        .iter()
        .copied()
        .filter(|b| str_at(b, "/spec/image/tag").trim() == tag)
        .collect();
    latest_build(&same_tag)
}

pub(super) fn sort_key(build: &Value) -> (String, String) {
    (
        str_at(build, "/metadata/creationTimestamp").to_string(),
        name(build).unwrap_or("").to_string(),
    )
}

/// Build child name: `<pipeline40>-<tag30>-<suffix>` (DNS-1123 label safe).
pub(super) fn build_name_with(pipeline: &str, tag: &str, suffix: &str) -> String {
    let head = truncate_slug(&slug(pipeline), 40);
    let head = if head.is_empty() {
        "pipeline".to_string()
    } else {
        head
    };
    let tag = truncate_slug(&slug(tag), 30);
    let tag = if tag.is_empty() {
        "build".to_string()
    } else {
        tag
    };
    format!("{head}-{tag}-{suffix}")
}

/// Lowercase `[a-z0-9-]` slug: runs collapse to one `-`, ends trimmed.
pub(super) fn slug(text: &str) -> String {
    let mut out = String::new();
    for ch in text.chars() {
        let c = ch.to_ascii_lowercase();
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            out.push(c);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_matches('-').to_string()
}

/// `slug` result limited to `max` chars, trailing `-` trimmed.
pub(super) fn truncate_slug(text: &str, max: usize) -> String {
    let cut: String = text.chars().take(max).collect();
    cut.trim_end_matches('-').to_string()
}

/// Set the image of `container` (or the first container when empty).
/// Returns false when the pod template has no such container.
pub(super) fn set_container_image(obj: &mut Value, container: &str, image: &str) -> bool {
    let Some(containers) = obj
        .pointer_mut("/spec/template/spec/containers")
        .and_then(Value::as_array_mut)
    else {
        return false;
    };
    let picked = if container.is_empty() {
        containers.first_mut()
    } else {
        containers
            .iter_mut()
            .find(|c| c.get("name").and_then(Value::as_str) == Some(container))
    };
    let Some(c) = picked else { return false };
    c["image"] = Value::String(image.to_string());
    true
}

/// `docker build` argv for `build`, tagged with `reference` and labeled with
/// the owning pipeline so the host daemon can be pruned back to
/// [`BUILD_IMAGE_RETENTION`] images (T8.3 bug fix B8).
pub(super) fn build_args(build: &Value, reference: &str) -> Vec<String> {
    let context = str_at(build, "/spec/source/context").trim();
    let mut args = vec!["build".to_string()];
    if let Some(dockerfile) = dockerfile_arg(build) {
        args.push("-f".to_string());
        args.push(dockerfile);
    }
    if let Some(label) = image_label_value(build) {
        args.push("--label".to_string());
        args.push(format!("{IMAGE_LABEL}={label}"));
    }
    args.push("-t".to_string());
    args.push(reference.to_string());
    for arg in string_array(build.pointer("/spec/args")) {
        if !arg.trim().is_empty() {
            args.push("--build-arg".to_string());
            args.push(arg);
        }
    }
    // docker's build context: "." when unset, else a trailing slash.
    let context = match context {
        "" => ".".to_string(),
        c if c.ends_with('/') => c.to_string(),
        c => format!("{c}/"),
    };
    args.push(context);
    args
}

/// Absolute Dockerfile path: `spec.source.dockerfile` joined onto the
/// context unless it is already absolute (empty = docker's default).
pub(super) fn dockerfile_arg(build: &Value) -> Option<String> {
    let dockerfile = str_at(build, "/spec/source/dockerfile").trim();
    if dockerfile.is_empty() {
        return None;
    }
    if dockerfile.starts_with('/') {
        return Some(dockerfile.to_string());
    }
    let context = str_at(build, "/spec/source/context")
        .trim()
        .trim_end_matches('/');
    Some(format!("{context}/{dockerfile}"))
}

/// Per-build docker budget: `spec.timeoutSeconds` when set, else the config.
pub(super) fn timeout_for(build: &Value, default_seconds: u64) -> Duration {
    let secs = build
        .pointer("/spec/timeoutSeconds")
        .and_then(Value::as_u64)
        .filter(|s| *s > 0)
        .unwrap_or(default_seconds);
    Duration::from_secs(secs)
}

/// Concatenate two command logs, skipping empty halves.
pub(super) fn join_logs(first: &str, second: &str) -> String {
    match (first.is_empty(), second.is_empty()) {
        (true, true) => String::new(),
        (true, false) => second.to_string(),
        (false, true) => first.to_string(),
        (false, false) => format!("{first}\n{second}"),
    }
}

/// Missing-or-empty JSON string (an unset `status` field).
pub(super) fn blank(value: Option<&Value>) -> bool {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("")
        .is_empty()
}

// ============================ host image hygiene ============================
// T8.3 bug fix (B8): every Build used to leave its image on the host docker
// daemon, so node disk only ever grew. Images are now labeled with the
// pipeline that built them, older ones are pruned back to
// `BUILD_IMAGE_RETENTION` before a build, and the just-pushed copy is
// dropped after a successful push (the registry owns it by then). All of it
// is best effort: the argv + the retention selection live here (pure), the
// `executor::run` calls live in `build.rs`.

/// Label tying a host docker image to the pipeline whose Build produced it.
pub(super) const IMAGE_LABEL: &str = "init-pro.pipeline";

/// Host images kept per pipeline (older ones are pruned before a build).
pub(super) const BUILD_IMAGE_RETENTION: usize = 3;

/// `"<ns>/<pipeline>"` stamped as [`IMAGE_LABEL`] on a Build's image
/// (`None` when the Build carries no identity: nothing to select by).
pub(super) fn image_label_value(build: &Value) -> Option<String> {
    let ns = namespace(build).filter(|ns| !ns.is_empty())?;
    let pipeline = str_at(build, "/spec/pipeline").trim();
    (!pipeline.is_empty()).then(|| format!("{ns}/{pipeline}"))
}

/// `docker images` argv: the pipeline's images, newest first (docker's
/// default order), one `<ID>\t<repo:tag>` row per image.
pub(super) fn list_images_args(label_value: &str) -> Vec<String> {
    vec![
        "images".to_string(),
        "--filter".to_string(),
        format!("label={IMAGE_LABEL}={label_value}"),
        "--format".to_string(),
        "{{.ID}}\t{{.Repository}}:{{.Tag}}".to_string(),
    ]
}

/// `docker rmi` argv for one image handle (a `repo:tag` or an image ID).
pub(super) fn rmi_args(handle: &str) -> Vec<String> {
    vec!["rmi".to_string(), handle.to_string()]
}

/// Images to prune out of a `docker images` listing: every row past the
/// newest `keep`. Rows that do not parse (the `$ docker ...` log header,
/// daemon warnings) are ignored; an untagged `<none>` reference falls back
/// to the image ID so the prune still has something to remove.
pub(super) fn prune_targets(listing: &str, keep: usize) -> Vec<String> {
    listing
        .lines()
        .filter_map(image_row_handle)
        .skip(keep)
        .collect()
}

/// `rmi` handle for one listing row (`ID<TAB>repo:tag`), `None` when the
/// line is not an image row.
fn image_row_handle(line: &str) -> Option<String> {
    let mut fields = line.split('\t');
    let id = fields.next()?.trim();
    let reference = fields.next().unwrap_or_default().trim();
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric()) {
        return None; // log header, warning, empty line
    }
    let untagged = reference.is_empty() || reference.contains("<none>");
    Some(if untagged {
        id.to_string()
    } else {
        reference.to_string()
    })
}

/// True when `reference` is the image the owning pipeline currently runs
/// (`status.lastBuild.image`): that host copy stays warm, the registry
/// copy alone may be enough for the rest.
pub(super) fn is_promoted(reference: &str, promoted: &str) -> bool {
    !promoted.is_empty() && reference == promoted
}

#[cfg(test)]
mod tests;
