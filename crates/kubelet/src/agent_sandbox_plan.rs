//! T7.7 (Q63) sandbox attempt planning: the PURE spec->container mapping
//! (decode, attempt id, in-container argv/env plan) plus the runner
//! staging into the private rootfs. Split from `agent_exec_sandbox.rs`
//! to respect the 400-line file cap; no behavior change.
//!
//! Long-run sweep planners (C14) live here too because they are pure:
//! the per-attempt dir layout + the stale-dir plan (H4) and the Pending
//! retry backoff curve (H2).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;

use crate::agent_objects as objs;
use crate::agent_sandbox::{sanitize_attempt_id, RUNNER_TARGET, WORKSPACE_TARGET};
use api::agent_policy::{
    AgentPolicySpec, AgentSandboxSpec, EffectivePolicy, SANDBOX_AUDIT_FILE, SANDBOX_POLICY_FILE,
};

/// Decode one optional spec field. `None` = absent (or null) => the
/// legacy Q37 path; `Some(Err)` = malformed => deterministic `*SpecInvalid`.
fn decode_field<T: serde::de::DeserializeOwned>(
    raw: &Value,
    pointer: &str,
    name: &str,
) -> Option<Result<T, String>> {
    let node = raw.pointer(pointer)?;
    if node.is_null() {
        return None;
    }
    Some(serde_json::from_value(node.clone()).map_err(|e| format!("{name}: {e}")))
}

/// `/spec/sandbox` (hardened containment request, Q63).
pub(crate) fn decode_sandbox(raw: &Value) -> Option<Result<AgentSandboxSpec, String>> {
    decode_field(raw, "/spec/sandbox", "spec.sandbox")
}

/// `/spec/policy` (optional narrowing overlay, Q63).
pub(crate) fn decode_policy(raw: &Value) -> Option<Result<AgentPolicySpec, String>> {
    decode_field(raw, "/spec/policy", "spec.policy")
}

/// Deterministic per-object attempt id (Q63): sanitized name + the last 8
/// chars of the object uid — a recreated object (new uid) gets a new id;
/// the same object re-run REUSES the id and stale sockets are removed
/// best-effort at spawn.
pub(crate) fn attempt_id(view: &objs::AgentView) -> String {
    let skip = view.uid.chars().count().saturating_sub(8);
    let suffix: String = view.uid.chars().skip(skip).collect();
    format!(
        "{}-{}",
        sanitize_attempt_id(&view.name),
        sanitize_attempt_id(&suffix)
    )
}

/// T7.7 D7 (Q63): the in-container argv/env for the HARDENED
/// `agent-runner` — no longer the legacy opencode CLI. The whole tool
/// registry runs in-process behind the central gate; `--policy/--audit`
/// wire the fail-closed policy file + the advisory denial audit (both
/// under `/workspace/.init-pro/`, see [`write_policy_file`]), and
/// `--attempt` doubles as the session id. Env stays the closed
/// HOME/XDG/PATH triple (+ the broker socket via `sanitize_env`).
pub(crate) fn container_plan(
    view: &objs::AgentView,
    prompt_file: Option<&Path>,
    hardened_base: &str,
    attempt: &str,
) -> (Vec<String>, Vec<(String, String)>) {
    let mut argv = vec![
        format!("{RUNNER_TARGET}/{hardened_base}"),
        "--workdir".to_string(),
        WORKSPACE_TARGET.to_string(),
        "--policy".to_string(),
        SANDBOX_POLICY_FILE.to_string(),
        "--audit".to_string(),
        SANDBOX_AUDIT_FILE.to_string(),
        "--attempt".to_string(),
        attempt.to_string(),
    ];
    if let Some(m) = &view.model {
        argv.extend(["--model".to_string(), m.clone()]);
    }
    if let Some(a) = &view.agent {
        argv.extend(["--agent".to_string(), a.clone()]);
    }
    if let Some(pf) = prompt_file {
        let name = pf
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "prompt.md".to_string());
        argv.extend([
            "--prompt-file".to_string(),
            format!("{WORKSPACE_TARGET}/{name}"),
        ]);
    }
    argv.push(view.prompt.clone());
    let envs = vec![
        ("HOME".into(), WORKSPACE_TARGET.to_string()),
        ("XDG_DATA_HOME".into(), format!("{WORKSPACE_TARGET}/mem")),
        (
            "PATH".into(),
            format!("{WORKSPACE_TARGET}/tools:{RUNNER_TARGET}"),
        ),
    ];
    (argv, envs)
}

/// Stage the operator runner dir INTO the private rootfs at `/runner`
/// (Q63): nsrun `:ro` binds are NOEXEC (`harden_mount`), so a read-only
/// bind can never exec the runner — root-owned copies in the private
/// rootfs keep the runner unmodifiable by the sandbox uid (by ownership)
/// AND executable. Per-attempt private bytes.
pub(crate) fn stage_runner_into_rootfs(rootfs: &Path, runner: &Path) -> std::io::Result<()> {
    let src_dir = runner.parent().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "runner has no parent dir")
    })?;
    let dst_dir = rootfs.join(RUNNER_TARGET.strip_prefix('/').unwrap_or(RUNNER_TARGET));
    fn copy_tree(src: &Path, dst: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(dst)?;
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            let from = entry.path();
            let to = dst.join(entry.file_name());
            if entry.file_type()?.is_dir() {
                copy_tree(&from, &to)?;
            } else {
                std::fs::copy(&from, &to)?;
            }
        }
        Ok(())
    }
    copy_tree(src_dir, &dst_dir)
}

/// Host-side path of one in-container `/workspace/...` path (T7.7 D7):
/// strips the workspace mount prefix and re-joins it on the host dir.
fn host_path_for(ws_host: &Path, in_container: &str) -> PathBuf {
    let rel = in_container
        .strip_prefix(WORKSPACE_TARGET)
        .unwrap_or(in_container)
        .trim_start_matches('/');
    ws_host.join(rel)
}

/// T7.7 D7 (Q63): write the attempt's [`EffectivePolicy`] (the camelCase
/// wire form, Q10) to `<ws>/.init-pro/policy.json` — the file the
/// hardened `agent-runner` reads fail-closed at
/// `/workspace/.init-pro/policy.json`. `0644`: world-readable (the
/// sandbox uid must read it), owner-writable (the node owns the bytes);
/// policy content is never a secret.
pub(crate) fn write_policy_file(ws_host: &Path, eff: &EffectivePolicy) -> std::io::Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let path = host_path_for(ws_host, SANDBOX_POLICY_FILE);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let body = serde_json::to_vec(eff)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(&path, body)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))?;
    Ok(path)
}

/// `<data>/agent-sandbox/`: the root of the per-attempt private trees.
pub(crate) const AGENT_SANDBOX_DIR: &str = "agent-sandbox";
/// Per-attempt subdirs (Q63): the private rootfs and the writable scratch.
pub(crate) const ROOTFS_SUBDIR: &str = "rootfs";
pub(crate) const SCRATCH_SUBDIR: &str = "scratch";
const ATTEMPT_SUBDIRS: [&str; 2] = [ROOTFS_SUBDIR, SCRATCH_SUBDIR];

/// H4 (C14): the per-attempt dir `<base>/agent-sandbox/<id>` (`base` =
/// node data dir; the id goes through the same sanitize rules as the
/// engine socket name, so `<id>.sock` and the dir always agree). This is
/// the ONLY tree a finished attempt deletes: the workspace root
/// (`<base>/agent-ws/`, pinned per-address NFS mounts) and the runner
/// staging are not under it.
pub(crate) fn attempt_dir(base: &Path, id: &str) -> PathBuf {
    base.join(AGENT_SANDBOX_DIR).join(sanitize_attempt_id(id))
}

/// H4 (C14): what a finished attempt deletes, in order: `rootfs/`, then
/// `scratch/` (both recursive), then the attempt dir itself -- only once
/// empty, so anything an operator parked next to them is never touched.
pub(crate) fn attempt_cleanup_paths(base: &Path, id: &str) -> (Vec<PathBuf>, PathBuf) {
    let dir = attempt_dir(base, id);
    let subdirs = ATTEMPT_SUBDIRS.iter().map(|s| dir.join(s)).collect();
    (subdirs, dir)
}

/// H4 (C14): the pure boot-sweep plan. `found` = entry names under
/// `<data>/agent-sandbox/`; `live` = attempt ids whose engine socket
/// `<data>/run/engine/<id>.sock` still exists (teardown unlinks it, and
/// a tied engine unlinks it on the parent-gone exit path). Everything
/// found but not live is stale, sorted and deduplicated.
pub(crate) fn stale_attempt_dirs(found: &[String], live: &[String]) -> Vec<String> {
    let live: BTreeSet<&str> = live.iter().map(String::as_str).collect();
    let mut stale: Vec<String> = found
        .iter()
        .filter(|f| !live.contains(f.as_str()))
        .cloned()
        .collect();
    stale.sort();
    stale.dedup();
    stale
}

/// H2 (C14): base and cap of the Pending retry backoff.
const RETRY_BASE: Duration = Duration::from_millis(500);
const RETRY_CAP: Duration = Duration::from_secs(300);

/// H2 (C14): how long to wait before re-attempting an Agent that stayed
/// Pending (`SandboxRequired`/`SandboxUnavailable`): `failures` is the
/// number of CONSECUTIVE Pending outcomes so far (>= 1). 500 ms doubling
/// per failure, capped at 5 min -- `500ms, 1s, 2s, ... 5m, 5m`.
pub(crate) fn retry_delay(failures: u32) -> Duration {
    let shift = failures.saturating_sub(1).min(10);
    RETRY_BASE.saturating_mul(1u32 << shift).min(RETRY_CAP)
}

/// H2 (C14): pure half of the repeat detection -- the `reason` of the
/// `Ready=False` condition of an already-written Pending status body
/// (`None` when the body is not a Pending-with-Ready shape).
pub(crate) fn pending_reason(body: &Value) -> Option<&str> {
    let status = body.pointer("/status")?;
    if status.pointer("/phase").and_then(Value::as_str) != Some(objs::PHASE_PENDING) {
        return None;
    }
    status
        .pointer("/conditions")?
        .as_array()?
        .iter()
        .find(|c| c.get("type").and_then(Value::as_str) == Some(objs::COND_READY))
        .and_then(|c| c.get("reason"))
        .and_then(Value::as_str)
}

/// Advisory post-verdict denial count (T7.7 D7, Q63): lines of the
/// in-workspace audit JSONL whose `type` is `ActionDenied` AND whose
/// `attempt` matches this attempt. The file is agent-tamperable by
/// design (it lives in the writable workspace) — the node-side verdict
/// stays authoritative, so the count is advisory only. `None` = absent
/// or unreadable (nothing to report).
pub(crate) fn audit_denial_count(ws_host: &Path, attempt: &str) -> Option<usize> {
    let body = std::fs::read_to_string(host_path_for(ws_host, SANDBOX_AUDIT_FILE)).ok()?;
    let is_denial = |line: &str| -> bool {
        serde_json::from_str::<Value>(line)
            .map(|v| {
                v.get("type").and_then(Value::as_str) == Some("ActionDenied")
                    && v.get("attempt").and_then(Value::as_str) == Some(attempt)
            })
            .unwrap_or(false)
    };
    Some(body.lines().filter(|l| is_denial(l.trim())).count())
}

#[cfg(test)] // H2/H4 (C14) pure planner cases split out (file size limit)
#[path = "agent_sandbox_plan_tests.rs"]
mod plan_tests;
