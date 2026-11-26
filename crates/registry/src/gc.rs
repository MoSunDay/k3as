//! Retention enforcement + space reclamation (TODO **T6.3**, Q40).
//!
//! `apply_retention` runs after every manifest push: it asks policy.rs
//! for the prune plan, drops the tag records, removes repo manifests
//! that lost their last referencing tag, then reclaims unreferenced
//! blob files (with a freshness grace window so a concurrent
//! blob-upload-then-manifest-push pair is never collected mid-flight).
//! Failures are logged and swallowed (log-and-retry doctrine): a GC
//! miss only delays reclamation, it never fails the client push.
//!
//! Upload staging files (bug fix **B9**): [`sweep_stale_uploads`] rides
//! the SAME GC pass - every manifest push - and reclaims `uploads/*`
//! staging files older than [`STALE_UPLOAD_SECS`]. There is no dedicated
//! timer/thread for it: the only long-running loop in this crate is the
//! axum serve task, and a push-heavy registry is exactly the one that
//! accumulates abandoned sessions. The synchronous unlink in
//! `uploads::append_body` handles the common case (a failed PATCH), the
//! boot-time [`sweep_uploads`] handles a killed server, and this sweep is
//! the belt-and-braces for anything that slipped between them.

use std::collections::HashSet;
use std::io;
use std::path::Path;
use std::time::{Duration, SystemTime};

use crate::policy;
use crate::store;

/// Blob grace window: blobs younger than this are never GC'd even if
/// unreferenced (their manifest push may be one round-trip away).
pub const BLOB_GC_GRACE_SECS: u64 = 600;

/// Upload staging files older than this are treated as abandoned (B9): a
/// resumable PATCH session lasts seconds-to-minutes, never hours.
pub const STALE_UPLOAD_SECS: u64 = 3600;

/// Upper bound on entries inspected per sweep, so one GC pass stays cheap
/// even if a misbehaving client litters the uploads dir.
const MAX_SWEEP_ENTRIES: usize = 1024;

/// Enforce tag retention for one repository, then GC blobs globally.
pub async fn apply_retention(
    root: &Path,
    name: &str,
    keep_extra: &[String],
    retain: usize,
) -> io::Result<()> {
    // B9: the stale-upload sweep rides this pass (it runs on every
    // manifest push, i.e. exactly when upload sessions exist) and must not
    // be short-circuited by an empty prune plan below.
    sweep_stale_uploads(root, STALE_UPLOAD_SECS).await;
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

/// Remove every upload staging file (boot-time sweep; an interrupted
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

/// Remove upload staging files whose mtime is older than `older_than_secs`
/// and return how many were dropped (B9). Bounded by [`MAX_SWEEP_ENTRIES`]
/// per pass; failures are logged at debug and swallowed (log-and-retry
/// doctrine). Returns the count so callers/tests can assert the outcome.
pub async fn sweep_stale_uploads(root: &Path, older_than_secs: u64) -> usize {
    let cutoff = SystemTime::now()
        .checked_sub(Duration::from_secs(older_than_secs))
        .unwrap_or(SystemTime::UNIX_EPOCH);
    let Ok(mut rd) = tokio::fs::read_dir(root.join("uploads")).await else {
        return 0; // no uploads dir -> nothing to reclaim
    };
    let mut removed = 0usize;
    let mut seen = 0usize;
    while let Ok(Some(entry)) = rd.next_entry().await {
        seen += 1;
        if seen > MAX_SWEEP_ENTRIES {
            tracing::debug!(
                target: "init-pro",
                limit = MAX_SWEEP_ENTRIES,
                "registry: stale upload sweep hit its per-pass bound"
            );
            break;
        }
        let path = entry.path();
        let Ok(meta) = entry.metadata().await else {
            continue;
        };
        if !meta.is_file() {
            continue; // never recurse: subdirs are not staging files
        }
        // A fresh mtime means a live (or just-finished) session.
        let fresh = meta.modified().map(|m| m > cutoff).unwrap_or(true);
        if fresh {
            continue;
        }
        match tokio::fs::remove_file(&path).await {
            Ok(()) => removed += 1,
            Err(e) => tracing::debug!(
                target: "init-pro",
                path = %path.display(),
                error = %e,
                "registry: stale upload removal failed"
            ),
        }
    }
    if removed > 0 {
        tracing::debug!(
            target: "init-pro",
            removed,
            older_than_secs,
            "registry: stale upload sessions swept"
        );
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Scratch registry root for one test (unique per process+test name).
    async fn scratch(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "init-pro-registry-gc-{}-{name}",
            std::process::id()
        ));
        let _ = tokio::fs::remove_dir_all(&root).await;
        tokio::fs::create_dir_all(&root).await.unwrap();
        store::ensure_uploads(&root).await.unwrap();
        root
    }

    /// Rewind a file's mtime by `secs` (the "abandoned session" shape).
    fn age(path: &Path, secs: u64) {
        let f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        f.set_modified(SystemTime::now() - Duration::from_secs(secs))
            .unwrap();
    }

    /// B9: old staging files go, fresh ones survive, the pass is idempotent.
    #[tokio::test]
    async fn stale_uploads_are_swept_and_fresh_ones_survive() {
        let root = scratch("stale-sweep").await;
        let stale = store::upload_file(&root, "stale-1");
        let fresh = store::upload_file(&root, "fresh-1");
        tokio::fs::write(&stale, b"abandoned").await.unwrap();
        tokio::fs::write(&fresh, b"in-flight").await.unwrap();
        age(&stale, STALE_UPLOAD_SECS * 2);

        assert_eq!(sweep_stale_uploads(&root, STALE_UPLOAD_SECS).await, 1);
        assert!(
            !stale.exists(),
            "an abandoned staging file survived the sweep"
        );
        assert!(fresh.exists(), "a live upload session was swept");
        // Second pass: nothing left to reclaim.
        assert_eq!(sweep_stale_uploads(&root, STALE_UPLOAD_SECS).await, 0);
        let _ = tokio::fs::remove_dir_all(&root).await;
    }

    #[tokio::test]
    async fn sweep_tolerates_a_missing_uploads_dir() {
        let root =
            std::env::temp_dir().join(format!("init-pro-registry-gc-{}-none", std::process::id()));
        let _ = tokio::fs::remove_dir_all(&root).await;
        assert_eq!(sweep_stale_uploads(&root, STALE_UPLOAD_SECS).await, 0);
    }
}
