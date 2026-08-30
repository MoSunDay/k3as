//! Embedded anyhome wiring (TODO **T-anyhome**, decision **Q36**): the
//! NFSv3 memory service behind Workflow/Agent workspaces, served
//! in-process by `init-pro server` on ONE port (NFS + MOUNT + portmap
//! multiplexed by nfsserve). Assembly mirrors the upstream recipe in
//! `link_repos/any-home/crates/anyhome/src/main.rs`: `RealFs` over
//! `<data-dir>/anyhome/fs`, `MemoryDb` at `<data-dir>/anyhome/anyhome.db`,
//! `DummyEmbedder` at the upstream `--dim` default, export `/` so a client
//! mounts `host:/` and walks `/workflow/<id>` + `/agent/<id>` (the paths
//! pinned by `common::workspace`).
//!
//! v1 assumption (Q36): bind == advertise -- the service binds
//! `--anyhome-addr` and that SAME address is advertised into pod volume
//! sources and Agent workspace annotations; the multi-node split is Phase B.
//! Known nfsserve shutdown race on stop: an upstream coordination item; data
//! safety is unaffected (WAL + fsync).
//!
//! Shutdown: nfsserve has no cancel seam, so the serve loop is a
//! `tokio::select!` between `listener.handle_forever()` and the shared
//! [`infra::Shutdown`] token; dropping the listener future closes the
//! listening socket (accepted connections wind down with the task). Before
//! returning, the MemoryDb WAL is checkpointed with the upstream three-arm
//! logging (a failed checkpoint loses nothing -- writes are already
//! fsynced).
#![forbid(unsafe_code)]

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::Context;
use nfsserve::tcp::{NFSTcp, NFSTcpListener};
use storage::StorageBackend;
use tokio::task::JoinHandle;

/// The anyhome export subtrees pinned by `common::workspace` (Q36): created
/// eagerly at startup so `/workflow/...` and `/agent/...` exist even before
/// the keeper sees the first object.
const WORKFLOW_ROOT: &str = "workflow";
/// See [`WORKFLOW_ROOT`].
const AGENT_ROOT: &str = "agent";

/// Embedding dimensionality of the embedded embedder (T-anyhome, Q36).
///
/// The embedder's dimension dictates the MemoryDb schema (the embedding
/// column is `FLOAT32(dim)`) and MemoryDb *validates dim on reopen*, so the
/// persisted `anyhome.db` pins this number: it MUST stay the upstream
/// anyhome `--dim` default. The unit test below parses the upstream `Cli`
/// and asserts equality, so upstream drift fails our build loudly instead
/// of corrupting on-disk databases at runtime.
pub const EMBED_DIM: usize = 64;

/// `<data-dir>/anyhome/fs`: the directory anyhome exposes as `/`.
pub fn fs_root(data_dir: &Path) -> PathBuf {
    data_dir.join("anyhome/fs")
}

/// `<data-dir>/anyhome/anyhome.db`: the MemoryDb (libSQL) file.
pub fn db_path(data_dir: &Path) -> PathBuf {
    data_dir.join("anyhome/anyhome.db")
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

/// Create the anyhome fs root plus the `workflow/` and `agent/` export
/// subtrees (Q36 layout). Idempotent: existing directories are kept, so
/// both the eager pre-flight in [`spawn_anyhome`] and [`serve_anyhome`]
/// can call it.
pub fn prepare_fs_roots(root: &Path) -> io::Result<()> {
    std::fs::create_dir_all(root)?;
    std::fs::create_dir_all(root.join(WORKFLOW_ROOT))?;
    std::fs::create_dir_all(root.join(AGENT_ROOT))?;
    Ok(())
}

/// Assemble and serve anyhome until [`infra::Shutdown`] fires (T-anyhome,
/// Q36). Mirrors upstream `main.rs` layer-for-layer; see the module docs.
pub async fn serve_anyhome(
    data_dir: &Path,
    addr: &str,
    shutdown: infra::Shutdown,
) -> anyhow::Result<()> {
    let root = fs_root(data_dir);
    prepare_fs_roots(&root).with_context(|| format!("anyhome fs root {}", root.display()))?;
    let real = anyhome::RealFs::new(root.clone()).context("anyhome RealFs init")?;
    // MemoryDb::open validates EMBED_DIM against the persisted schema.
    let db = anyhome::MemoryDb::open(&db_path(data_dir).to_string_lossy(), EMBED_DIM)
        .await
        .context("anyhome MemoryDb open")?;
    // Clone for the graceful-shutdown checkpoint; MemoryDb is cheaply Clone
    // (shares the one connection) and `db` itself moves into AnyHomeFs.
    let db_shutdown = db.clone();
    tracing::warn!(
        target: "init-pro",
        "using DummyEmbedder -- /search does exact-match only, no real semantics"
    );
    let embedder: Arc<dyn anyhome::db::Embedder> =
        Arc::new(anyhome::db::DummyEmbedder::new(EMBED_DIM));
    let fs = anyhome::AnyHomeFs::new(real, db, embedder);
    let mut listener = NFSTcpListener::bind(addr, fs)
        .await
        .with_context(|| format!("anyhome bind {addr}"))?;
    // Export `/`: a client mounts `host:/` and walks /workflow + /agent.
    listener.with_export_name("/");
    tracing::info!(
        target: "init-pro",
        addr = %addr,
        port = anyhome::cli::port_of(addr),
        export = "/",
        fs_root = %root.display(),
        "NFSv3 listening. mount: mount.nfs -o vers=3,tcp,nolock,noacl,port={p},mountport={p} localhost:/ /mnt/anyhome",
        p = anyhome::cli::port_of(addr),
    );
    tokio::select! {
        // Dropping this future (the shutdown arm wins the select) closes
        // the listening socket: nfsserve exposes no cancel seam.
        res = listener.handle_forever() => {
            res.context("anyhome nfs listener")?;
        }
        _ = shutdown.cancelled() => {}
    }
    // Converge the WAL into the main DB file so a plain copy of anyhome.db
    // is a complete backup (upstream main.rs semantics: writes are fsynced,
    // so a failed checkpoint loses nothing -- it only leaves WAL frames).
    match db_shutdown.checkpoint().await {
        Ok((0, wal_frames, checkpointed)) => {
            tracing::info!(target: "init-pro", wal_frames, checkpointed,
                "WAL checkpoint complete (TRUNCATE)");
        }
        Ok((busy, wal_frames, checkpointed)) => {
            tracing::warn!(target: "init-pro", busy, wal_frames, checkpointed,
                "WAL checkpoint blocked by a concurrent reader (PASSIVE fallback); WAL retains frames");
        }
        Err(e) => {
            tracing::warn!(target: "init-pro", error = %e,
                "WAL checkpoint failed; committed data remains safe in the WAL");
        }
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

    // clap::Parser supplies `try_parse_from` for the upstream anyhome Cli.
    use clap::Parser as _;

    fn tmp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("init-pro-anyhome-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Upstream drift guard: EMBED_DIM must equal the upstream anyhome
    /// `--dim` default (MemoryDb validates dim on reopen; a silent upstream
    /// change would corrupt existing anyhome.db files).
    #[test]
    fn embed_dim_matches_upstream_cli_default() {
        let upstream = anyhome::cli::Cli::try_parse_from(["anyhome"]).unwrap();
        assert_eq!(EMBED_DIM, upstream.dim);
    }

    #[test]
    fn paths_follow_anyhome_layout() {
        let dd = Path::new("/tmp/dd");
        assert_eq!(fs_root(dd), Path::new("/tmp/dd/anyhome/fs"));
        assert_eq!(db_path(dd), Path::new("/tmp/dd/anyhome/anyhome.db"));
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
    fn prepare_fs_roots_creates_workflow_and_agent_trees() {
        let dd = tmp_dir("roots");
        let root = dd.join("anyhome/fs");
        prepare_fs_roots(&root).unwrap();
        assert!(root.is_dir());
        assert!(root.join(WORKFLOW_ROOT).is_dir());
        assert!(root.join(AGENT_ROOT).is_dir());
        // Idempotent: a second pass over an existing tree succeeds.
        prepare_fs_roots(&root).unwrap();
        assert!(root.join(WORKFLOW_ROOT).is_dir());
        assert!(root.join(AGENT_ROOT).is_dir());
        let _ = std::fs::remove_dir_all(&dd);
    }
}
