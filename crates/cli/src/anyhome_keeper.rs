//! Workspace keeper for the embedded anyhome NFS plane (TODO **T-anyhome**,
//! decision **Q36**; per-Pod workspaces TODO **T7.5**, decision **Q43**).
//!
//! The SERVER holds the storage `Arc`, so the keeper polls the store
//! directly (Q19: no loopback HTTP, no token) and pre-creates one directory
//! per object under the anyhome fs root -- `<fs>/workflow/<id>`,
//! `<fs>/agent/<id>`, and (T7.5, Q43) `<fs>/pod/<ns>/<name>` plus every
//! `init-pro.io/anyhome-mounts` server path (e.g. `<fs>/team/shared`) --
//! so a container mounting its workspace before the first write finds it.
//!
//! The export paths ARE `common::workspace` (the SSOT, T-anyhome/Q36 and
//! T7.5/Q43): the keeper calls it directly, so the guard and the path
//! shapes cannot drift from the Workflow controller's /workspace
//! auto-mount injection, the apiserver's per-Pod anyhome defaulting, or
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
//! work. Pod dirs join the same no-GC policy (T7.5, Q43): a deleted Pod's
//! workspace and every custom shared path it referenced SURVIVE. Split out
//! of [`crate::anyhome`] for the same reason as [`crate::flannel_keeper`]:
//! both files stay under the 400-line cap.
//!
//! Cadence: one full-collection LIST per beat per collection (now also
//! Pods) is the v1 shape (T7.5, Q43); watch is the future optimization.
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
/// Pod collection (core group, namespaced, T7.5/Q43): the EMPTY group
/// string is how core-group keys are built everywhere (see the scheduler
/// and controllers wiring); all namespaces in one LIST.
fn pod_prefix() -> KeyPrefix {
    KeyPrefix::new("", "pods", None)
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

/// Export-path directories a stored-Pod page needs under the fs root
/// (T7.5, Q43): (a) the Pod's own `pod_workspace_path(ns, name)` when
/// `init-pro.io/anyhome` parses to `Rw`/`Ro` (`Off`/invalid skip silently
/// -- the keeper is advisory; the apiserver already rejected invalid
/// create), and (b) every `server_path` of a well-formed `anyhome-mounts`
/// value (`Err` skips the annotation). Sorted + deduped like
/// [`names_from`]; empty/missing namespace or name skips the Pod.
pub fn pod_dirs_from(items: &[serde_json::Value]) -> Vec<String> {
    use common::workspace::{AnyhomeMode, ANYHOME_ANNOTATION, ANYHOME_MOUNTS_ANNOTATION};
    let mut dirs: Vec<String> = Vec::new();
    for o in items {
        let Some(m) = o.get("metadata") else { continue };
        let ns = m.get("namespace").and_then(|v| v.as_str()).unwrap_or("");
        let name = m.get("name").and_then(|v| v.as_str()).unwrap_or("");
        if ns.is_empty() || name.is_empty() {
            continue;
        }
        let annotations = m.get("annotations");
        let mode = annotations
            .and_then(|a| a.get(ANYHOME_ANNOTATION))
            .and_then(|v| v.as_str())
            .and_then(common::workspace::parse_anyhome_mode);
        if matches!(mode, Some(AnyhomeMode::Rw | AnyhomeMode::Ro)) {
            dirs.extend(common::workspace::pod_workspace_path(ns, name));
        }
        let mounts = annotations
            .and_then(|a| a.get(ANYHOME_MOUNTS_ANNOTATION))
            .and_then(|v| v.as_str())
            .map(common::workspace::parse_anyhome_mounts);
        if let Some(Ok(mounts)) = mounts {
            dirs.extend(mounts.into_iter().map(|m| m.server_path));
        }
    }
    dirs.sort();
    dirs.dedup();
    dirs
}

/// Materialize one workspace directory per id (T-anyhome, Q36) plus one
/// per Pod export path (T7.5, Q43).
///
/// Ids are sanitized through the `common::workspace` SSOT guards first:
/// `None` (empty id or id containing `/` -- the only path-significant
/// character) is skipped WITHOUT failing the batch, so one bad object
/// cannot wedge the keeper. `pod_dirs` are full export paths
/// (`/pod/<ns>/<name>`, `/team/shared`) from [`pod_dirs_from`]; deletion
/// is ignored (A5) and `create_dir_all` is a no-op on existing dirs.
pub fn sync_workspaces(
    fs_root: &Path,
    workflow_ids: &[String],
    agent_ids: &[String],
    pod_dirs: &[String],
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
            // Agent workspaces additionally get the shared persona/tools
            // bytes (Q37): `skills/`, `tools/`, `mem/` plus a `prompt.md`
            // template -- created ONLY when absent, never overwritten (the
            // user edits persona bytes through the NFS mount; the runner's
            // node-local `prepare_workspace` only adds the per-node
            // `.opencoder` symlink on top).
            if kind == "agent" {
                scaffold_agent_workspace(&dir)?;
            }
        }
    }
    // Pod workspaces and custom shared paths (T7.5, Q43): plain mkdir -p,
    // no scaffold -- the bytes belong to the pod/container, not the keeper.
    for export_path in pod_dirs {
        let dir = workspace_dir(fs_root, export_path);
        std::fs::create_dir_all(&dir)
            .map_err(|e| format!("pod workspace dir {}: {e}", dir.display()))?;
    }
    Ok(())
}

/// Lay down the shared agent-workspace bytes (create-if-absent). Fails the
/// batch on a real I/O error, like the directory materialization above.
fn scaffold_agent_workspace(dir: &Path) -> Result<(), String> {
    for sub in ["skills", "tools", "mem"] {
        std::fs::create_dir_all(dir.join(sub))
            .map_err(|e| format!("agent workspace dir {}: {e}", dir.join(sub).display()))?;
    }
    let prompt = dir.join("prompt.md");
    if !prompt.exists() {
        std::fs::write(&prompt, AGENT_PROMPT_TEMPLATE)
            .map_err(|e| format!("agent workspace {}: {e}", prompt.display()))?;
    }
    Ok(())
}

/// Seed `prompt.md` (Q37). Presence of this file makes the runner pass
/// `--prompt-file` (the persona); `spec.prompt` stays the positional task.
/// Kept tiny and self-explanatory; never re-written once it exists.
const AGENT_PROMPT_TEMPLATE: &str = "# Agent persona

Edit this file to define the agent persona (system prompt). It is passed to the runner via --prompt-file; spec.prompt remains the per-run task.
";

/// Join an export path (`/workflow/<id>`) onto the fs root. The export path
/// always starts at `/`, so the join replaces the root instead of climbing
/// out of it.
fn workspace_dir(fs_root: &Path, export_path: &str) -> PathBuf {
    fs_root.join(export_path.trim_start_matches('/'))
}

/// Keeper loop (T-anyhome, Q36; pods T7.5, Q43): list Workflows + Agents +
/// annotated Pods across ALL namespaces each beat and converge the
/// workspace tree. Syncs FIRST (a tree that matches the boot-time store
/// state appears immediately), then sleeps; bails as soon as the daemon's
/// alive flag drops -- a dead NFS plane must not keep conjuring
/// directories. Store errors are retried next beat, never fatal.
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
        let mut pod_dirs = Vec::new();
        // (prefix, sink, extractor): pure page -> directory-inputs fold (T7.5/Q43).
        type Extract = fn(&[serde_json::Value]) -> Vec<String>;
        let pages: [(KeyPrefix, &mut Vec<String>, Extract); 3] = [
            (workflow_prefix(), &mut workflow_ids, names_from),
            (agent_prefix(), &mut agent_ids, names_from),
            (pod_prefix(), &mut pod_dirs, pod_dirs_from),
        ];
        for (prefix, out, extract) in pages {
            match store.list(&prefix).await {
                Ok(entries) => {
                    let values: Vec<serde_json::Value> =
                        entries.into_iter().map(|e| e.value).collect();
                    *out = extract(&values);
                }
                Err(e) => {
                    tracing::warn!(target: "init-pro", error = %e,
                        "workspace keeper: list failed (retrying next beat)");
                }
            }
        }
        if let Err(e) = sync_workspaces(&fs_root, &workflow_ids, &agent_ids, &pod_dirs) {
            tracing::warn!(target: "init-pro", error = %e, "workspace keeper: sync failed");
            continue;
        }
        tracing::debug!(target: "init-pro", workflows = workflow_ids.len(), agents = agent_ids.len(),
            pod_dirs = pod_dirs.len(), "workspace keeper: synced");
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
            &["hello-wf".to_string()],
            &["agent-1".to_string()],
            &[],
        )
        .unwrap();
        assert!(root.join("workflow/hello-wf").is_dir());
        assert!(root.join("agent/agent-1").is_dir());
        // The SSOT guard's documented edge: `..` is a legal in-name id and
        // normalizes away under the root, not out of it.
        sync_workspaces(&root, &[], &["..".to_string()], &[]).unwrap();
        assert!(root.join("agent/..").is_dir());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn sync_creates_nested_dirs_for_valid_ids() {
        let root = tmp_dir("sync");
        sync_workspaces(
            &root,
            &["probe-wf".to_string()],
            &["agent-1".to_string()],
            &[],
        )
        .unwrap();
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
            &[],
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
    fn agent_workspaces_get_shared_scaffold() {
        let root = tmp_dir("scaffold");
        sync_workspaces(&root, &[], &["a1".to_string()], &[]).unwrap();
        let dir = workspace_dir(&root, "/agent/a1");
        for sub in ["skills", "tools", "mem"] {
            assert!(dir.join(sub).is_dir(), "missing {sub}");
        }
        let prompt = dir.join("prompt.md");
        assert!(prompt.is_file());
        // Re-sync must NOT overwrite user-edited bytes.
        std::fs::write(&prompt, b"# mine").unwrap();
        sync_workspaces(&root, &[], &["a1".to_string()], &[]).unwrap();
        assert_eq!(std::fs::read(&prompt).unwrap(), b"# mine");
    }

    #[test]
    fn workflow_workspaces_are_not_agent_scaffolded() {
        let root = tmp_dir("no-scaffold");
        sync_workspaces(&root, &["w1".to_string()], &[], &[]).unwrap();
        let dir = workspace_dir(&root, "/workflow/w1");
        assert!(!dir.join("prompt.md").exists());
        assert!(!dir.join("skills").exists());
    }

    #[test]
    fn sync_is_idempotent() {
        let root = tmp_dir("idem");
        sync_workspaces(&root, &["w1".to_string()], &["a1".to_string()], &[]).unwrap();
        let marker = root.join("workflow/w1/keep.txt");
        std::fs::write(&marker, b"mem").unwrap();
        sync_workspaces(&root, &["w1".to_string()], &["a1".to_string()], &[]).unwrap();
        assert_eq!(std::fs::read(&marker).unwrap(), b"mem");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn deletion_is_deliberately_ignored() {
        // A5: the id disappears from the next beat, the directory (and its
        // memory) survives.
        let root = tmp_dir("gc");
        sync_workspaces(&root, &["gone-wf".to_string()], &[], &[]).unwrap();
        let dir = root.join("workflow/gone-wf");
        std::fs::write(dir.join("memory.txt"), b"still here").unwrap();
        sync_workspaces(&root, &["other-wf".to_string()], &[], &[]).unwrap();
        assert!(dir.join("memory.txt").is_file());
        assert!(root.join("workflow/other-wf").is_dir());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A stored Pod (core group, T7.5/Q43); `None` = annotation absent.
    fn pod(ns: &str, name: &str, mode: Option<&str>, mounts: Option<&str>) -> serde_json::Value {
        let mut annotations = serde_json::Map::new();
        for (key, value) in [
            (common::workspace::ANYHOME_ANNOTATION, mode),
            (common::workspace::ANYHOME_MOUNTS_ANNOTATION, mounts),
        ] {
            if let Some(value) = value {
                annotations.insert(key.to_string(), value.into());
            }
        }
        serde_json::json!({"metadata": {"name": name, "namespace": ns, "annotations": annotations}})
    }

    #[test]
    fn pod_dirs_from_extracts_workspace_and_custom_paths() {
        let items = [
            // rw Pod: own workspace + two custom shared server paths.
            pod(
                "default",
                "web",
                Some("rw"),
                Some("/team/shared:/shared:ro,/cache:/cache"),
            ),
            // ro Pod: workspace only.
            pod("prod", "worker", Some("ro"), None),
            // A second Pod sharing `/cache`: deduped below.
            pod("other", "twin", Some("true"), Some("/cache:/c")),
            // Off / invalid mode: skipped silently (advisory keeper; the
            // apiserver rejects these at create time).
            pod("default", "off", Some("false"), None),
            pod("default", "bad", Some("banana"), None),
            // Invalid mounts value: that annotation is dropped; the valid
            // mode still yields the workspace.
            pod("default", "badmounts", Some("rw"), Some("nope:/x")),
            // No annotations / missing namespace / no metadata: skipped.
            pod("default", "plain", None, None),
            serde_json::json!({"metadata": {"name": "no-ns"}, "spec": {}}),
            serde_json::json!({"spec": {}}),
        ];
        assert_eq!(
            pod_dirs_from(&items).join(" "),
            "/cache /pod/default/badmounts /pod/default/web /pod/other/twin /pod/prod/worker /team/shared"
        );
    }

    #[test]
    fn pod_dirs_and_custom_paths_materialize_idempotently() {
        let root = tmp_dir("pods");
        let pod_dirs = vec![
            common::workspace::pod_workspace_path("default", "web").expect("valid metadata"),
            "/team/shared".to_string(),
        ];
        sync_workspaces(&root, &[], &[], &pod_dirs).unwrap();
        assert!(root.join("pod/default/web").is_dir());
        assert!(root.join("team/shared").is_dir());
        // Idempotent: a re-sync keeps bytes written through the mount.
        let marker = root.join("pod/default/web/keep.txt");
        std::fs::write(&marker, b"mem").unwrap();
        sync_workspaces(&root, &[], &[], &pod_dirs).unwrap();
        assert_eq!(std::fs::read(&marker).unwrap(), b"mem");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn pod_dir_deletion_is_deliberately_ignored() {
        // A5 (T7.5, Q43): the Pod disappears from the next beat; its
        // workspace AND the custom shared path it referenced survive.
        let root = tmp_dir("pod-gc");
        let dirs = ["/pod/default/gone".to_string(), "/team/shared".to_string()];
        sync_workspaces(&root, &[], &[], &dirs).unwrap();
        let ws = root.join("pod/default/gone");
        std::fs::write(ws.join("memory.txt"), b"still here").unwrap();
        sync_workspaces(&root, &[], &[], &["/pod/default/next".to_string()]).unwrap();
        assert!(ws.join("memory.txt").is_file());
        assert!(root.join("team/shared").is_dir());
        assert!(root.join("pod/default/next").is_dir());
        let _ = std::fs::remove_dir_all(&root);
    }
}
