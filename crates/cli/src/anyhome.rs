//! Embedded anyhome wiring (TODO **T-anyhome**, decisions **Q36** and
//! **Q66**): the NFSv3 workspace plane behind Workflow/Agent/Pod
//! workspaces, served in-process by `init-pro server` on ONE port (NFS +
//! MOUNT + portmap multiplexed by nfsserve). Since Q66 (S54) the
//! implementation base is the in-tree directory VFS
//! [`crate::anyhome_dirfs::DirFs`] (adapted from nfsserve's mirrorfs
//! example) mounted directly on `NFSTcpListener` over
//! `<data-dir>/anyhome/fs` and exported at `/`, so a client mounts
//! `host:/` and walks `/workflow/<id>` + `/agent/<id>` +, since T7.5
//! (Q43), `/pod/<ns>/<name>` (the paths pinned by `common::workspace`).
//! The any-home crate assembly (`RealFs` + `MemoryDb` +
//! `DummyEmbedder`, the `anyhome.db` file) is decommissioned — its
//! retrieval layer never had a consumer in init-pro (Q66).
//!
//! v1 assumption (Q36): bind == advertise -- the service binds
//! `--anyhome-addr` and that SAME address is advertised into pod volume
//! sources and Agent workspace annotations; the multi-node split is
//! Phase B.
//!
//! Shutdown: nfsserve has no cancel seam, so the serve loop is a
//! `tokio::select!` between `listener.handle_forever()` and the shared
//! [`infra::Shutdown`] token; dropping the listener future closes the
//! listening socket (accepted connections wind down with the task).
#![forbid(unsafe_code)]

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::Context;
// NFSTcp is the trait supplying `handle_forever` on the bound listener.
use nfsserve::tcp::{NFSTcp, NFSTcpListener};
use storage::StorageBackend;
use tokio::task::JoinHandle;

use crate::anyhome_dirfs::DirFs;

/// The anyhome export subtrees pinned by `common::workspace` (Q36; the
/// pod root T7.5, Q43): created eagerly at startup so `/workflow/...`,
/// `/agent/...` and `/pod/...` exist even before the keeper sees the first
/// object.
const WORKFLOW_ROOT: &str = "workflow";
/// See [`WORKFLOW_ROOT`].
const AGENT_ROOT: &str = "agent";
/// Per-Pod workspace root (T7.5, Q43): the relative-segment spelling of
/// the SSOT [`common::workspace::POD_WORKSPACE_ROOT`] (`/pod`), kept local
/// to match the [`WORKFLOW_ROOT`]/[`AGENT_ROOT`] join idiom above.
const POD_ROOT: &str = "pod";

/// `<data-dir>/anyhome/fs`: the directory DirFs exposes as `/`.
pub fn fs_root(data_dir: &Path) -> PathBuf {
    data_dir.join("anyhome/fs")
}

/// `--disable-anyhome`/`--anyhome-addr` -> the wired endpoint (T-anyhome,
/// Q36). `None` = disabled (the NFS plane stays down); otherwise the
/// address is passed through verbatim, so wildcard or port-0 values keep
/// working.
pub fn anyhome_endpoint(disabled: bool, addr: &str) -> Option<String> {
    if disabled {
        None
    } else {
        Some(addr.to_string())
    }
}

/// Create the anyhome fs root plus the `workflow/`, `agent/` and `pod/`
/// export subtrees (Q36 layout; `pod/` added by T7.5, Q43, so
/// `/pod/<ns>/<name>` workspaces have their parent tree before the keeper
/// sees the first annotated Pod). Idempotent: existing directories are
/// kept, so both the eager pre-flight in [`spawn_anyhome`] and
/// [`serve_anyhome`] can call it.
pub fn prepare_fs_roots(root: &Path) -> io::Result<()> {
    std::fs::create_dir_all(root)?;
    std::fs::create_dir_all(root.join(WORKFLOW_ROOT))?;
    std::fs::create_dir_all(root.join(AGENT_ROOT))?;
    std::fs::create_dir_all(root.join(POD_ROOT))?;
    Ok(())
}

/// The port component of a `host:port` anyhome address (Q66: the
/// upstream `anyhome::cli::port_of` helper died with the entity; the
/// mount hint below still wants the bare port).
fn port_of(addr: &str) -> &str {
    addr.rsplit_once(':').map(|(_, port)| port).unwrap_or(addr)
}

/// Assemble and serve anyhome until [`infra::Shutdown`] fires (T-anyhome,
/// Q36; Q66 DirFs base -- see the module docs).
pub async fn serve_anyhome(
    data_dir: &Path,
    addr: &str,
    shutdown: infra::Shutdown,
) -> anyhow::Result<()> {
    let root = fs_root(data_dir);
    prepare_fs_roots(&root).with_context(|| format!("anyhome fs root {}", root.display()))?;
    let dirfs = DirFs::new(root.clone());
    let mut listener = NFSTcpListener::bind(addr, dirfs)
        .await
        .with_context(|| format!("anyhome bind {addr}"))?;
    // Export `/`: a client mounts `host:/` and walks /workflow + /agent.
    listener.with_export_name("/");
    tracing::info!(
        target: "init-pro",
        addr = %addr,
        export = "/",
        fs_root = %root.display(),
        "NFSv3 listening. mount: mount.nfs -o vers=3,tcp,nolock,noacl,port={p},mountport={p} localhost:/ /mnt/anyhome",
        p = port_of(addr),
    );
    tokio::select! {
        // Dropping this future (the shutdown arm wins the select) closes
        // the listening socket: nfsserve exposes no cancel seam.
        res = listener.handle_forever() => {
            res.context("anyhome nfs listener")?;
        }
        _ = shutdown.cancelled() => {}
    }
    Ok(())
}

/// Spawn the embedded anyhome NFS plane as a supervised task (T-anyhome,
/// Q36). Mirrors [`crate::flannel::spawn_flannel`]: the fs layout is
/// pre-created eagerly (an unwritable data-dir fails at boot, not lazily),
/// an `Arc<AtomicBool>` alive flag flips false on ANY daemon exit
/// (including error), the workspace keeper is spawned against the SAME
/// storage Arc (Q19: no HTTP hop, no token), and the DAEMON handle is
/// returned for the drain sequence.
pub fn spawn_anyhome(
    data_dir: &Path,
    addr: &str,
    store: Arc<dyn StorageBackend>,
    shutdown: infra::Shutdown,
) -> io::Result<JoinHandle<()>> {
    let root = fs_root(data_dir);
    prepare_fs_roots(&root)?;
    let addr = addr.to_string();
    let data_dir = data_dir.to_path_buf();
    let alive = Arc::new(AtomicBool::new(true));
    let alive_for_daemon = alive.clone();
    let daemon = tokio::spawn(async move {
        if let Err(e) = serve_anyhome(&data_dir, &addr, shutdown).await {
            tracing::warn!(target: "init-pro", error = %e, "anyhome failed (workspace plane degraded)");
        }
        alive_for_daemon.store(false, Ordering::Relaxed);
    });
    crate::anyhome_keeper::spawn_workspace_keeper(store, root, alive);
    Ok(daemon)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("init-pro-anyhome-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn paths_follow_anyhome_layout() {
        let dd = Path::new("/tmp/dd");
        assert_eq!(fs_root(dd), Path::new("/tmp/dd/anyhome/fs"));
    }

    #[test]
    fn endpoint_flag_wiring() {
        assert_eq!(
            anyhome_endpoint(false, "127.0.0.1:11111"),
            Some("127.0.0.1:11111".to_string())
        );
        assert_eq!(
            anyhome_endpoint(false, "0.0.0.0:0"),
            Some("0.0.0.0:0".to_string())
        );
        assert_eq!(anyhome_endpoint(true, "127.0.0.1:11111"), None);
    }

    #[test]
    fn prepare_fs_roots_creates_workflow_agent_and_pod_trees() {
        let dd = tmp_dir("roots");
        let root = dd.join("anyhome/fs");
        prepare_fs_roots(&root).unwrap();
        assert!(root.is_dir());
        assert!(root.join(WORKFLOW_ROOT).is_dir());
        assert!(root.join(AGENT_ROOT).is_dir());
        assert!(root.join(POD_ROOT).is_dir());
        // SSOT drift guard (T7.5, Q43): the eager root is exactly the
        // `common::workspace::POD_WORKSPACE_ROOT` export root.
        assert_eq!(
            POD_ROOT,
            common::workspace::POD_WORKSPACE_ROOT.trim_start_matches('/')
        );
        // Idempotent: a second pass over an existing tree succeeds.
        prepare_fs_roots(&root).unwrap();
        assert!(root.join(WORKFLOW_ROOT).is_dir());
        assert!(root.join(AGENT_ROOT).is_dir());
        assert!(root.join(POD_ROOT).is_dir());
        let _ = std::fs::remove_dir_all(&dd);
    }
}
