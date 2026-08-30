//! Workspace keeper for the embedded anyhome NFS plane (TODO **T-anyhome**,
//! decision **Q36**).
//!
//! The SERVER holds the storage `Arc`, so the keeper polls the store
//! directly (Q19: no loopback HTTP, no token) and pre-creates one directory
//! per Workflow/Agent under the anyhome fs root -- `<fs>/workflow/<id>` and
//! `<fs>/agent/<id>` -- so a container mounting its workspace before the
//! first write finds the directory already there.
//!
//! The export paths ARE `common::workspace` (the SSOT, T-anyhome/Q36):
//! the keeper calls it directly, so the guard and the path shapes cannot
//! drift from the Workflow controller's /workspace auto-mount injection or
//! Phase B agent execution. The SSOT deliberately lives in `common`, NOT in
//! `agent`: `agent` pulls opencoder-session -> opencoder-store -> libsql
//! 0.9.30 while anyhome vendors libsql 0.10.0-pre.4, and the two copies
//! collide at link time (`duplicate symbol: update_hook_cb`) in EVERY
//! binary that links cli -- so no crate on the binary's dependency path may
//! depend on `agent` (T-anyhome finding), while `common` is dependency-free.
//!
//! Deletion is DELIBERATELY ignored (escalation item **A5**): memory
//! persistence beats GC. An object deleted (or a namespace torn down) keeps
//! its directory and its contents, because the whole point of the memory
//! plane is that bytes outlive the object that produced them; a garbage
//! collector here would need an owner/subresource model to avoid eating
//! shared scratch space, which is upstream-coordination work, not keeper
//! work. Split out of [`crate::anyhome`] for the same reason as
//! [`crate::flannel_keeper`]: both files stay under the 400-line cap.
#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use storage::{KeyPrefix, StorageBackend};

/// Workflow collection (argoproj.io/v1alpha1, namespaced, Q30); `None`
/// namespace = ALL namespaces in one LIST (storage key layout, T2.2).
fn workflow_prefix() -> KeyPrefix {
    KeyPrefix::new("argoproj.io", "workflows", None)
}
/// Agent collection (init-pro.io/v1, namespaced, Q35); all namespaces.
fn agent_prefix() -> KeyPrefix {
    KeyPrefix::new("init-pro.io", "agents", None)
}

/// Poll cadence: one beat is one cheap LIST per collection while the tree
/// is in sync (5s, the same cadence as the flannel condition keeper).
const POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Workspace ids (`metadata.name`) carried by a stored-object page:
/// sorted + deduped so keeper logs are stable across beats.
pub fn names_from(items: &[serde_json::Value]) -> Vec<String> {
    let mut names: Vec<String> = items
        .iter()
        .filter_map(|o| o.get("metadata")?.get("name")?.as_str().map(str::to_string))
        .collect();
    names.sort();
    names.dedup();
    names
}

/// Materialize one workspace directory per id (T-anyhome, Q36).
///
/// Ids are sanitized through the `common::workspace` SSOT guards first:
/// `None` (empty id or id containing `/` -- the only path-significant
/// character) is skipped WITHOUT failing the batch, so one bad object
/// cannot wedge the keeper. Deletion is ignored (see the module docs, A5).
/// Idempotent: `create_dir_all` on an existing directory is a no-op.
pub fn sync_workspaces(
    fs_root: &Path,
    workflow_ids: &[String],
    agent_ids: &[String],
) -> Result<(), String> {
    for (kind, ids) in [("workflow", workflow_ids), ("agent", agent_ids)] {
        for id in ids {
            let export_path = match kind {
                "workflow" => common::workspace::workflow_workspace_path(id),
                _ => common::workspace::agent_workspace_path(id),
            };
            let export_path = match export_path {
                Some(p) => p,
                // Invalid id shape: skip, do not fail the batch.
                None => continue,
            };
            let dir = workspace_dir(fs_root, &export_path);
            std::fs::create_dir_all(&dir)
                .map_err(|e| format!("workspace dir {}: {e}", dir.display()))?;
        }
    }
    Ok(())
}

/// Join an export path (`/workflow/<id>`) onto the fs root. The export path
/// always starts at `/`, so the join replaces the root instead of climbing
/// out of it.
fn workspace_dir(fs_root: &Path, export_path: &str) -> PathBuf {
    fs_root.join(export_path.trim_start_matches('/'))
}

/// Keeper loop (T-anyhome, Q36): list Workflows + Agents across ALL
/// namespaces each beat and converge the workspace tree. Syncs FIRST (a
/// tree that matches the boot-time store state appears immediately), then
/// sleeps; bails as soon as the daemon's alive flag drops -- a dead NFS
/// plane must not keep conjuring directories. Store errors are logged and
/// retried next beat, never fatal: the keeper is advisory.
pub async fn run_workspace_keeper(
    store: Arc<dyn StorageBackend>,
    fs_root: PathBuf,
    alive: Arc<AtomicBool>,
) {
    let mut first = true;
    loop {
        if !first {
            tokio::time::sleep(POLL_INTERVAL).await;
        }
        first = false;
        if !alive.load(Ordering::Relaxed) {
            break;
        }
        let mut workflow_ids = Vec::new();
        let mut agent_ids = Vec::new();
        let pages = [
            (workflow_prefix(), &mut workflow_ids),
            (agent_prefix(), &mut agent_ids),
        ];
        for (prefix, ids) in pages {
            match store.list(&prefix).await {
                Ok(entries) => {
                    let values: Vec<serde_json::Value> =
                        entries.into_iter().map(|e| e.value).collect();
                    *ids = names_from(&values);
                }
                Err(e) => {
                    tracing::warn!(target: "init-pro", error = %e,
                        "workspace keeper: list failed (retrying next beat)");
                }
            }
        }
        if let Err(e) = sync_workspaces(&fs_root, &workflow_ids, &agent_ids) {
            tracing::warn!(target: "init-pro", error = %e, "workspace keeper: sync failed");
            continue;
        }
        tracing::debug!(target: "init-pro", workflows = workflow_ids.len(), agents = agent_ids.len(),
            "workspace keeper: synced");
    }
}

/// Spawn the keeper as a detached task (T-anyhome, Q36): its lifetime is
/// tied to the daemon's alive flag, not to a drain handle (the same shape
/// as the flannel condition keeper).
pub(crate) fn spawn_workspace_keeper(
    store: Arc<dyn StorageBackend>,
    fs_root: PathBuf,
    alive: Arc<AtomicBool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(run_workspace_keeper(store, fs_root, alive))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "init-pro-anyhome-keeper-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn wf(name: &str) -> serde_json::Value {
        serde_json::json!({
            "apiVersion": "argoproj.io/v1alpha1",
            "kind": "Workflow",
            "metadata": {"name": name, "namespace": "default"},
            "spec": {}
        })
    }

    #[test]
    fn names_are_sorted_and_deduped() {
        let items = [wf("b"), wf("a"), wf("a"), serde_json::json!({"spec": {}})];
        assert_eq!(names_from(&items), vec!["a".to_string(), "b".to_string()]);
    }

    /// Keeper/SSOT integration pin: `sync_workspaces` routes through
    /// `common::workspace` (the relocated SSOT), so the on-disk tree uses
    /// exactly the SSOT path shapes -- `<root>/workflow/<id>` and
    /// `<root>/agent/<id>` -- including the `..`-is-allowed edge the SSOT
    /// tests pin (a separator-free id cannot escape the prefix).
    #[test]
    fn keeper_tree_uses_common_workspace_ssot_paths() {
        let root = tmp_dir("ssot");
        sync_workspaces(
            &root,
            &["hello-workflow".to_string()],
            &["agent-1".to_string()],
        )
        .unwrap();
        assert!(root.join("workflow/hello-workflow").is_dir());
        assert!(root.join("agent/agent-1").is_dir());
        // The SSOT guard's documented edge: `..` is a legal in-name id and
        // normalizes away under the root, not out of it.
        sync_workspaces(&root, &[], &["..".to_string()]).unwrap();
        assert!(root.join("agent/..").is_dir());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn sync_creates_nested_dirs_for_valid_ids() {
        let root = tmp_dir("sync");
        sync_workspaces(&root, &["probe-wf".to_string()], &["agent-1".to_string()]).unwrap();
        assert!(root.join("workflow/probe-wf").is_dir());
        assert!(root.join("agent/agent-1").is_dir());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn invalid_ids_are_skipped_without_failing_the_batch() {
        let root = tmp_dir("badids");
        // empty + '/'-containing ids: rejected by the common::workspace guard.
        sync_workspaces(
            &root,
            &[
                "".to_string(),
                "ok/wf".to_string(),
                "../etc".to_string(),
                "good-wf".to_string(),
            ],
            &[String::new(), "a/b".to_string(), "good-agent".to_string()],
        )
        .unwrap();
        assert!(root.join("workflow/good-wf").is_dir());
        assert!(root.join("agent/good-agent").is_dir());
        assert!(!root.join("workflow/ok").exists());
        assert!(!root.join("workflow/etc").exists());
        assert_eq!(std::fs::read_dir(root.join("workflow")).unwrap().count(), 1);
        assert_eq!(std::fs::read_dir(root.join("agent")).unwrap().count(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn sync_is_idempotent() {
        let root = tmp_dir("idem");
        sync_workspaces(&root, &["w1".to_string()], &["a1".to_string()]).unwrap();
        let marker = root.join("workflow/w1/keep.txt");
        std::fs::write(&marker, b"mem").unwrap();
        sync_workspaces(&root, &["w1".to_string()], &["a1".to_string()]).unwrap();
        assert_eq!(std::fs::read(&marker).unwrap(), b"mem");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn deletion_is_deliberately_ignored() {
        // A5: the id disappears from the next beat, the directory (and its
        // memory) survives.
        let root = tmp_dir("gc");
        sync_workspaces(&root, &["gone-wf".to_string()], &[]).unwrap();
        let dir = root.join("workflow/gone-wf");
        std::fs::write(dir.join("memory.txt"), b"still here").unwrap();
        sync_workspaces(&root, &["other-wf".to_string()], &[]).unwrap();
        assert!(dir.join("memory.txt").is_file());
        assert!(root.join("workflow/other-wf").is_dir());
        let _ = std::fs::remove_dir_all(&root);
    }
}
