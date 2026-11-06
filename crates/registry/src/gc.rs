//! Retention enforcement + space reclamation (TODO **T6.3**, Q40).
//!
//! `apply_retention` runs after every manifest push: it asks policy.rs
//! for the prune plan, drops the tag records, removes repo manifests
//! that lost their last referencing tag, then reclaims unreferenced
//! blob files (with a freshness grace window so a concurrent
//! blob-upload-then-manifest-push pair is never collected mid-flight).
//! Failures are logged and swallowed (log-and-retry doctrine): a GC
//! miss only delays reclamation, it never fails the client push.

use std::collections::HashSet;
use std::io;
use std::path::Path;

use crate::policy;
use crate::store;

/// Blob grace window: blobs younger than this are never GC'd even if
/// unreferenced (their manifest push may be one round-trip away).
pub const BLOB_GC_GRACE_SECS: u64 = 600;

/// Enforce tag retention for one repository, then GC blobs globally.
pub async fn apply_retention(
    root: &Path,
    name: &str,
    keep_extra: &[String],
    retain: usize,
) -> io::Result<()> {
    let patterns = policy::load_keep(root, keep_extra).await;
    let tags = store::list_tags(root, name).await?;
    let drops = policy::prune_plan(name, &tags, &patterns, retain);
    if drops.is_empty() {
        return Ok(());
    }
    // Digests that lose a tag; a manifest dies only when NO surviving
    // tag (kept or forever) still references it.
    let dropped_digests: HashSet<String> = drops
        .iter()
        .filter_map(|t| {
            tags.iter()
                .find(|(x, _)| x == t)
                .map(|(_, m)| m.digest.clone())
        })
        .collect();
    for t in &drops {
        if let Err(e) = store::delete_tag(root, name, t).await {
            tracing::warn!(target: "init-pro", repo = name, tag = %t, error = %e, "registry: tag prune delete failed");
        }
    }
    let survivors: HashSet<String> = store::list_tags(root, name)
        .await?
        .into_iter()
        .map(|(_, m)| m.digest)
        .collect();
    for d in dropped_digests {
        if !survivors.contains(&d) {
            let _ = store::delete_manifest(root, name, &d).await;
        }
    }
    tracing::info!(
        target: "init-pro",
        repo = name,
        dropped = drops.len(),
        kept_forever = tags.len() - drops.len(),
        retain,
        "registry: tag retention applied"
    );
    gc_blobs(root).await
}

/// Reclaim blobs referenced by no stored manifest (grace-windowed).
pub async fn gc_blobs(root: &Path) -> io::Result<()> {
    let now = crate::now_millis() / 1000;
    match store::gc_blobs(root, BLOB_GC_GRACE_SECS, now).await {
        Ok(n) if n > 0 => {
            tracing::info!(target: "init-pro", removed = n, "registry: unreferenced blobs reclaimed");
        }
        Ok(_) => {}
        Err(e) => {
            tracing::warn!(target: "init-pro", error = %e, "registry: blob GC failed");
        }
    }
    Ok(())
}

/// Remove stale upload staging files (boot-time sweep; an interrupted
/// upload session is unrecoverable across restarts by design).
pub async fn sweep_uploads(root: &Path) {
    let dir = root.join("uploads");
    let Ok(mut rd) = tokio::fs::read_dir(&dir).await else {
        return;
    };
    let mut n = 0u32;
    while let Ok(Some(e)) = rd.next_entry().await {
        if tokio::fs::remove_file(e.path()).await.is_ok() {
            n += 1;
        }
    }
    if n > 0 {
        tracing::info!(target: "init-pro", swept = n, "registry: stale upload sessions removed");
    }
}
