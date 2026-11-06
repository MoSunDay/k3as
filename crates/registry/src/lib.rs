//! Embedded OCI/Docker image registry implementing the Docker Registry
//! HTTP API V2 (TODO **T6.3**, decision **Q40**).
//!
//! Ships inside the `init-pro server` process (the anyhome doctrine: an
//! embedded first-class plane, default-on, no external install). The wire
//! surface is the V2 protocol docker/containerd/crictl already speak:
//! ping, catalog, tags, manifests, blob CAS + resumable uploads. On top
//! of the protocol sits the tag-retention plane: every manifest push
//! enforces "keep the newest N tags per repository" (default N = 10,
//! `--registry-tag-retention`, 0 = unlimited) while keep-forever
//! patterns (`keep.json` + `--registry-keep-tag`) are excluded from the
//! count and never pruned (gc.rs / policy.rs).
//!
//! Storage layout under `<data_dir>/registry/`:
//! - `blobs/sha256-<hex>`     global content-addressed blob store
//! - `repos/<name>/manifests/` per-repo manifests by digest
//! - `repos/<name>/tags/`      per-repo tag -> {digest, mediaType, ...}
//! - `uploads/<uuid>`          in-flight upload staging files
//! - `keep.json`               keep-forever pattern list (hot-reloaded)

#![forbid(unsafe_code)]

pub mod api;
pub mod gc;
pub mod policy;
pub mod store;
pub mod uploads;
pub mod validate;

use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use infra::Shutdown;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

pub use api::v2_router;
pub use policy::DEFAULT_TAG_RETENTION;

/// Default listen address: `127.0.0.1:15000`. Port 5000 (the docker
/// convention) collides with macOS AirPlay and 15000 keeps the family
/// of init-pro control ports (6443 / 10250 / 11111) untouched.
pub const DEFAULT_ADDR: &str = "127.0.0.1:15000";

/// Retention + keep-forever configuration (T6.3, Q40).
///
/// `retain == 0` means unlimited. `keep_extra` patterns (from
/// `--registry-keep-tag`) are unioned with the on-disk `keep.json`
/// patterns on every GC pass, so both stay live without a restart.
#[derive(Clone, Debug)]
pub struct RegistryConfig {
    pub retain: usize,
    pub keep_extra: Vec<String>,
}

impl Default for RegistryConfig {
    fn default() -> Self {
        Self {
            retain: DEFAULT_TAG_RETENTION,
            keep_extra: Vec::new(),
        }
    }
}

/// Wall-clock milliseconds since the Unix epoch. The only time source
/// in the crate; `pushed_at` ordering for retention uses it.
pub fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Bind `addr`, serve the V2 router until `shutdown` fires (graceful:
/// in-flight requests drain, then the listener drops).
pub async fn serve_registry(
    root: PathBuf,
    addr: &str,
    cfg: RegistryConfig,
    shutdown: Shutdown,
) -> io::Result<()> {
    store::prepare(&root)?;
    gc::sweep_uploads(&root).await;
    let listener = TcpListener::bind(addr).await?;
    tracing::info!(
        target: "init-pro",
        addr,
        root = %root.display(),
        "registry (Docker Registry HTTP API v2) listening"
    );
    let app = v2_router(root, cfg);
    axum::serve(listener, app)
        .with_graceful_shutdown(async move { shutdown.cancelled().await })
        .await
        .map_err(io::Error::other)
}

/// Spawn the registry as a supervised task (T6.3, Q40). Mirrors the
/// anyhome/flannel doctrine: the fs layout is prepared eagerly so an
/// unwritable data-dir fails at boot, and a daemon error degrades the
/// image plane (warn) without taking the server down.
pub fn spawn_registry(
    data_dir: &Path,
    addr: &str,
    cfg: RegistryConfig,
    shutdown: Shutdown,
) -> io::Result<JoinHandle<()>> {
    let root = store::root_dir(data_dir);
    store::prepare(&root)?;
    let addr = addr.to_string();
    let data_root = root.clone();
    Ok(tokio::spawn(async move {
        if let Err(e) = serve_registry(data_root.clone(), &addr, cfg, shutdown).await {
            tracing::warn!(target: "init-pro", error = %e, addr, "registry failed (image plane degraded); another server may own the port; set --registry-addr");
        }
    }))
}
