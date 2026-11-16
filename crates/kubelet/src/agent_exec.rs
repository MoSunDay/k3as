//! One Agent execution attempt (TODO **T7.1** Phase B / B1, Q37): the
//! workspace work (NFS subpath mount, local scaffold), the subprocess wait
//! (timeout + delete-kill), and the merge-patch plumbing. Split from
//! [`crate::agents`] so the loops file and this file each stay under the
//! 400-line cap. Pure helpers for the argv/env mapping live in
//! [`crate::agent_objects`].

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::sync::{watch, Mutex};

use crate::agent_objects as objs;
use crate::agents::AgentRunnerConfig;
use crate::http::HttpJson;
use crate::nfs_mount;

/// Root under the node data dir holding per-Agent workspace mounts.
const WS_ROOT: &str = "agent-ws";

/// Last merge-patch body per key (write-if-changed dedup, shared by the
/// run loop and the execution attempts).
pub type Written = Arc<Mutex<BTreeMap<String, Value>>>;

/// Object-identity helper shared by the write-if-changed cache and the
/// `RunnerMissing` reported-set: `namespace/name` PLUS the object uid,
/// so a recreated same-name Agent (new uid, e.g. the F4 repro) is never
/// deduped against its predecessor's last body (or reported condition)
/// and silently left Pending without status.
pub(crate) fn dedupe_id(view: &objs::AgentView) -> String {
    format!("{}#{}", view.key, view.uid)
}

/// merge-patch the Agent status, write-if-changed. 404 (object deleted)
/// and other rejections are logged at debug and NOT retried here (the
/// watch re-list drives state); returns the response when applied.
pub async fn patch_if_changed(
    client: &HttpJson,
    written: &Written,
    view: &objs::AgentView,
    body: &Value,
) -> Option<Value> {
    {
        let id = dedupe_id(view);
        let mut w = written.lock().await;
        if w.get(&id) == Some(body) {
            return None;
        }
        w.insert(id, body.clone());
    }
    let path = format!(
        "/apis/init-pro.io/v1/namespaces/{ns}/agents/{name}",
        ns = view.namespace,
        name = view.name
    );
    match client.patch_json(&path, body).await {
        Ok((code, v)) if (200..300).contains(&code) => Some(v),
        Ok((code, _)) => {
            tracing::warn!(target: "init-pro", agent = %view.key, code, "agent status patch rejected");
            None
        }
        Err(e) => {
            tracing::warn!(target: "init-pro", agent = %view.key, error = %e, "agent status patch failed");
            None
        }
    }
}

/// A `Ready=False` condition (every runner-owned failure shape).
pub fn fail_cond(reason: &str, message: &str) -> Value {
    objs::condition(objs::COND_READY, "False", reason, message)
}

/// One full attempt: workspace -> spawn -> wait -> terminal condition.
/// Returns the terminal `(phase, condition)`, or `None` when the object
/// was deleted mid-flight (nothing left to patch).
pub(crate) async fn run_attempt(
    client: &HttpJson,
    cfg: &AgentRunnerConfig,
    written: &Written,
    record: &crate::agents::AgentRecord, // pub(crate) type
    keep_going: &mut watch::Receiver<bool>,
) -> Option<(&'static str, Value)> {
    let view = &record.view;
    let runner = cfg.runner_bin.as_ref()?;
    if let Err(reason) = objs::validate_prompt(view) {
        return Some((
            objs::PHASE_FAILED,
            fail_cond(reason, "spec.prompt is empty; nothing to execute"),
        ));
    }
    let source = objs::workspace_source(&record.raw, cfg.anyhome_addr.as_deref());
    let (ws, _mounted) = match &source {
        Some((addr, path)) => match mount_workspace(&cfg.data_dir, addr, path, &view.name).await {
            Ok(ws) => (ws, true),
            Err(e) => {
                tracing::warn!(target: "init-pro", agent = %view.key, error = %e, "workspace mount failed");
                return Some((objs::PHASE_FAILED, fail_cond("WorkspaceMountFailed", &e)));
            }
        },
        // Degraded: node-local memory, no NFS (Q37; keeper bytes stay shared).
        None => {
            tracing::warn!(target: "init-pro", agent = %view.key,
                "no workspace source (no annotation, no --anyhome-addr); using node-local dir");
            (workspace_dir(&cfg.data_dir, &view.name), false)
        }
    };
    if let Err(e) = prepare_workspace(&ws).await {
        return Some((objs::PHASE_FAILED, fail_cond("WorkspacePrepareFailed", &e)));
    }
    // Running: from here the attempt is observable.
    patch_if_changed(
        client,
        written,
        view,
        &objs::status_patch(
            objs::PHASE_RUNNING,
            &[objs::condition(
                objs::COND_READY,
                "True",
                "Executing",
                "runner started",
            )],
        ),
    )
    .await;
    let prompt_file = {
        let p = ws.join("prompt.md");
        if p.is_file() {
            Some(p)
        } else {
            None
        }
    };
    let plan = objs::build_spawn_plan(view, &ws, prompt_file, None);
    let mut cmd = tokio::process::Command::new(runner);
    cmd.args(&plan.args)
        .current_dir(&plan.workdir)
        .kill_on_drop(true)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in &plan.envs {
        cmd.env(k, v);
    }
    let wait_fut = async move {
        let output = cmd
            .spawn()
            .map_err(|e| format!("spawn {runner:?}: {e}"))?
            .wait_with_output()
            .await
            .map_err(|e| format!("wait: {e}"))?;
        Ok::<std::process::Output, String>(output)
    };
    let started = verdict(cfg, wait_fut, keep_going).await;
    match started {
        Verdict::Cancelled => None, // deleted mid-flight
        Verdict::Timeout => Some((
            objs::PHASE_FAILED,
            fail_cond(
                "ExecTimeout",
                &format!(
                    "execution exceeded {}s and was killed",
                    cfg.exec_timeout.as_secs()
                ),
            ),
        )),
        Verdict::Done(Err(e)) => Some((objs::PHASE_FAILED, fail_cond("RunnerError", &e))),
        Verdict::Done(Ok(out)) => {
            let session = objs::session_id_from_stderr(&String::from_utf8_lossy(&out.stderr));
            let session_note = session
                .map(|s| format!("; session {s}"))
                .unwrap_or_default();
            if out.status.success() {
                Some((
                    objs::PHASE_SUCCEEDED,
                    objs::condition(
                        objs::COND_READY,
                        "True",
                        "Completed",
                        &format!("exit 0{session_note}"),
                    ),
                ))
            } else {
                let code = out.status.code().unwrap_or(-1);
                Some((
                    objs::PHASE_FAILED,
                    fail_cond("RunnerFailed", &format!("exit {code}{session_note}")),
                ))
            }
        }
    }
}

/// How one attempt ended.
enum Verdict {
    Done(Result<std::process::Output, String>),
    Timeout,
    Cancelled,
}

/// Wait for the subprocess under BOTH the attempt timeout and the deletion
/// kill switch (`kill_on_drop` does the killing when the future is dropped).
async fn verdict(
    cfg: &AgentRunnerConfig,
    wait_fut: impl std::future::Future<Output = Result<std::process::Output, String>>,
    keep_going: &mut watch::Receiver<bool>,
) -> Verdict {
    let awaited = async {
        tokio::pin!(wait_fut);
        loop {
            tokio::select! {
                r = &mut wait_fut => break Verdict::Done(r),
                res = keep_going.changed() => match res {
                    Ok(()) if !*keep_going.borrow_and_update() => break Verdict::Cancelled,
                    Ok(()) => continue,
                    // All senders gone: nobody can cancel; wait it out.
                    Err(_) => break Verdict::Done(wait_fut.await),
                },
            }
        }
    };
    match tokio::time::timeout(cfg.exec_timeout, awaited).await {
        Ok(v) => v,
        Err(_) => Verdict::Timeout,
    }
}

/// Node-side mount target for one Agent workspace: `<data>/agent-ws/<name>`
/// (names are RFC1123 subdomains; `/` is rejected as defense in depth).
fn workspace_dir(data_dir: &Path, name: &str) -> PathBuf {
    if name.contains('/') {
        // Unreachable via the apiserver; refuse rather than escape.
        return data_dir.join(WS_ROOT).join("invalid");
    }
    data_dir.join(WS_ROOT).join(name)
}

/// How long one attempt waits for the mount to succeed before declaring
/// `WorkspaceMountFailed` (bounded; the target is the boot-time export
/// root, so the first try normally wins).
const MOUNT_WAIT: Duration = Duration::from_secs(20);

/// How long the liveness probe on an already-mounted root waits before
/// declaring it stale. A live NFS reply is sub-second; a stale NFSv3
/// `hard` mount never errors on its own (the RPC retries forever), so
/// the timeout is what converts "hung" into "recover".
const PROBE_WAIT: Duration = Duration::from_secs(3);

/// Node-side mount target for one anyhome ADDRESS: `<data>/agent-ws/nfs-<addr>`
/// (the addr's `:` becomes `_`; distinct addrs get distinct roots).
fn mount_root_dir(data_dir: &Path, addr: &str) -> PathBuf {
    let safe = addr.replace([':', '/'], "_");
    data_dir.join(WS_ROOT).join(format!("nfs-{safe}"))
}

/// What to do with the per-address root mount point.
#[derive(Debug, PartialEq, Eq)]
enum RootPlan {
    /// No mount there yet: mount (retrying) and pin.
    Mount,
    /// Mounted and live: reuse the pinned mount (deliberate, Q37).
    Reuse,
    /// Mounted but stale (the anyhome server restarted under it):
    /// lazy-unmount, then mount fresh.
    Recover,
}

/// Pure decision for the root mount point: unmounted -> [`RootPlan::Mount`];
/// mounted and the bounded liveness probe answered -> [`RootPlan::Reuse`];
/// mounted but the probe errored or timed out -> [`RootPlan::Recover`]
/// (a stale NFSv3 hard mount never errors on its own, so recovery must
/// be explicit; without it the pinned mount bricks every later Agent).
fn plan_root(mounts_text: &str, root: &Path, probe_live: bool) -> RootPlan {
    if !nfs_mount::is_mountpoint(mounts_text, root) {
        RootPlan::Mount
    } else if probe_live {
        RootPlan::Reuse
    } else {
        RootPlan::Recover
    }
}

/// Retry `mount.nfs` until `MOUNT_WAIT` (anyhome's port is up at boot;
/// transient EIO on the very first mount is the known F6 race).
async fn mount_with_retry(addr: &str, parent: &str, root: &Path) -> Result<(), String> {
    let deadline = std::time::Instant::now() + MOUNT_WAIT;
    loop {
        match nfs_mount::mount_nfs(Path::new("mount.nfs"), addr, parent, root).await {
            Ok(()) => return Ok(()),
            Err(e) if std::time::Instant::now() < deadline => {
                tracing::debug!(target: "init-pro", dir = %root.display(), error = %e,
                    "workspace root mount retrying");
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            Err(e) => return Err(e),
        }
    }
}

/// Mount the agent's anyhome workspace and return its per-Agent directory.
///
/// Upstream shape (Q37 coordination note): any-home's mountd resolves
/// export paths against the tree AS OF SERVER BOOT, and out-of-band
/// server-side writes are invisible to the plane — only bytes written
/// THROUGH a mount (or present at boot) exist in the NFS view. So the
/// per-Agent subpath `nfs://addr/agent/<name>` can NOT be mounted directly
/// (the keeper creates `<name>` after boot) and would not see keeper bytes
/// anyway. Instead we mount the PARENT export — `/agent`, pre-created at
/// boot by the server (Q36) — once per address, idempotent per
/// /proc/mounts, and return `<root>/<leaf>` as the workspace. The leaf is
/// created THROUGH the mount by [`prepare_workspace`], so it (and all
/// agent-written bytes) live in the plane and are visible to every node
/// mounting the same export. The root mount is deliberately left pinned
/// (it is per-address and reusable across Agents; the per-attempt
/// umount of the old per-Agent design would unmount siblings).
async fn mount_workspace(
    data_dir: &Path,
    addr: &str,
    path: &str,
    name: &str,
) -> Result<PathBuf, String> {
    // `nfs://addr/agent/<name>` -> mount `/agent`, work in `<name>`.
    let export = path.trim_end_matches('/');
    let (parent, leaf) = match export.rsplit_once('/') {
        Some((p, l)) if !l.is_empty() => {
            let parent = if p.is_empty() {
                "/".to_string()
            } else {
                p.to_string()
            };
            (parent, l.to_string())
        }
        _ => (export.to_string(), name.to_string()),
    };
    let root = mount_root_dir(data_dir, addr);
    // The pinned per-address root outlives agent restarts, and after an
    // anyhome server restart the NFSv3 hard mount is STALE: stat() on it
    // never completes, so create_dir_all surfaces the raw mkdir EEXIST
    // instead of recognizing the directory (F10). An existing root is
    // therefore not fatal here; liveness is decided by plan_root below.
    if let Err(e) = tokio::fs::create_dir_all(&root).await {
        if e.kind() != std::io::ErrorKind::AlreadyExists {
            return Err(format!("create {}: {e}", root.display()));
        }
    }
    let mounts = std::fs::read_to_string("/proc/self/mounts")
        .map_err(|e| format!("read /proc/self/mounts: {e}"))?;
    // Bounded liveness probe: a read through a live mount answers fast;
    // through a stale one it errors (ESTALE once the server is back) or
    // hangs until the timeout converts it into a Recover decision.
    let probe_live = matches!(
        tokio::time::timeout(PROBE_WAIT, tokio::fs::read_dir(&root)).await,
        Ok(Ok(_))
    );
    match plan_root(&mounts, &root, probe_live) {
        RootPlan::Reuse => {
            tracing::debug!(target: "init-pro", dir = %root.display(),
                "reusing pinned workspace root");
        }
        RootPlan::Recover => {
            // Lazy unmount detaches the dead filehandles without a server
            // round-trip (a plain umount could block on the stale RPC);
            // mount_with_retry then rebuilds a live mount in its place.
            tracing::warn!(target: "init-pro", dir = %root.display(),
                "workspace root stale (anyhome restarted?); lazy-unmounting and remounting");
            let _ = tokio::process::Command::new("umount")
                .arg("-l")
                .arg(&root)
                .output()
                .await;
            mount_with_retry(addr, &parent, &root).await?;
        }
        RootPlan::Mount => {
            mount_with_retry(addr, &parent, &root).await?;
        }
    }
    Ok(root.join(leaf))
}

/// Bounded mkdir retry for writes THROUGH a just-established NFS mount:
/// the first RPC round after `mount.nfs` returns can transiently fail
/// with EIO while anyhome's freshly restarted plane settles (F6 family);
/// three attempts half a second apart close it. `AlreadyExists`-like
/// outcomes are already Ok by create_dir_all semantics.
async fn create_dir_retry(dir: &Path) -> Result<(), String> {
    let mut last = String::new();
    for attempt in 0..3u32 {
        if let Err(e) = tokio::fs::create_dir_all(dir).await {
            last = format!("create {}: {e}", dir.display());
            tracing::debug!(target: "init-pro", attempt, dir = %dir.display(), error = %e,
                "workspace prepare retrying (F6 first-write race)");
            tokio::time::sleep(Duration::from_millis(500)).await;
            continue;
        }
        return Ok(());
    }
    Err(last)
}

/// Idempotent local scaffold mirroring the server-side keeper layout
/// (Q37): `skills/`, `tools/`, `mem/` and the `.opencoder/skills -> ../skills`
/// bridge that redirects opencoder's HOME-based skill lookup into the
/// shared export (A2).
async fn prepare_workspace(dir: &Path) -> Result<(), String> {
    for sub in ["skills", "tools", "mem", ".opencoder"] {
        create_dir_retry(&dir.join(sub)).await?;
    }
    let bridge = dir.join(".opencoder/skills");
    if let Err(e) = tokio::fs::symlink("../skills", &bridge).await {
        if e.kind() != std::io::ErrorKind::AlreadyExists {
            return Err(format!("symlink skills bridge: {e}"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_dir_is_per_agent_and_rejects_separators() {
        let data = Path::new("/d");
        assert_eq!(
            workspace_dir(data, "ag-1"),
            PathBuf::from("/d/agent-ws/ag-1")
        );
        assert_eq!(
            workspace_dir(data, "a/b"),
            PathBuf::from("/d/agent-ws/invalid")
        );
    }

    #[test]
    fn plan_root_mounts_when_absent_and_reuses_live() {
        let root = Path::new("/d/agent-ws/nfs-a_1");
        let text = "tmpfs / tmpfs rw 0 0\n";
        assert_eq!(plan_root(text, root, false), RootPlan::Mount);
        let text = "a:/agent /d/agent-ws/nfs-a_1 nfs rw,vers=3 0 0\n";
        assert_eq!(plan_root(text, root, true), RootPlan::Reuse);
    }

    #[test]
    fn plan_root_recovers_stale_pinned_mount() {
        let root = Path::new("/d/agent-ws/nfs-a_1");
        let text = "a:/agent /d/agent-ws/nfs-a_1 nfs rw,vers=3 0 0\n";
        assert_eq!(plan_root(text, root, false), RootPlan::Recover);
    }

    fn view_with(key: &str, uid: &str) -> objs::AgentView {
        objs::AgentView {
            key: key.to_string(),
            uid: uid.to_string(),
            name: "n".to_string(),
            namespace: "d".to_string(),
            node_name: None,
            phase: "Pending".to_string(),
            prompt: String::new(),
            model: None,
            agent: None,
        }
    }

    #[test]
    fn dedupe_id_is_uid_scoped_so_recreated_names_rewrite() {
        let a = view_with("d/n", "uid-1");
        let same = view_with("d/n", "uid-1");
        let recreated = view_with("d/n", "uid-2");
        assert_eq!(dedupe_id(&a), dedupe_id(&same));
        assert_ne!(dedupe_id(&a), dedupe_id(&recreated));
    }

    #[test]
    fn fail_cond_is_a_false_ready_condition() {
        let c = fail_cond("RunnerFailed", "exit 3");
        assert_eq!(c["type"], "Ready");
        assert_eq!(c["status"], "False");
        assert_eq!(c["reason"], "RunnerFailed");
        assert_eq!(c["message"], "exit 3");
    }

    #[test]
    fn exec_timeout_env_overrides_default() {
        let cfg =
            AgentRunnerConfig::new("http://127.0.0.1:1".into(), "n".into(), PathBuf::from("/d"));
        assert_eq!(
            cfg.exec_timeout,
            crate::agents::exec_timeout_from_env().unwrap_or(crate::agents::DEFAULT_EXEC_TIMEOUT)
        );
    }
}
