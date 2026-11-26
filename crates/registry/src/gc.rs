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
//!
//! Index children (batch D / **B1**): a multi-arch index names its platform
//! manifests by digest only, so retention (tags) never reached them and a
//! deleted index left its children -- and their config/layer blobs -- behind
//! forever. Every index deletion now cascades through
//! [`delete_with_children`]: a child goes when no surviving manifest of the
//! repo lists it and no tag points at it.

use std::collections::{HashMap, HashSet};
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
            delete_with_children(root, name, &d).await;
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

/// Deepest index-of-index nesting [`prune_index_children`] follows.
const MAX_INDEX_DEPTH: usize = 3;

/// Child manifest digests (`manifests[].digest`) listed by one stored
/// manifest; empty for an image manifest. Only grammatically valid digests
/// are returned: they become path segments downstream.
pub fn child_digests(manifest_bytes: &[u8]) -> Vec<String> {
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(manifest_bytes) else {
        return Vec::new();
    };
    v["manifests"]
        .as_array()
        .map(|kids| {
            kids.iter()
                .filter_map(|c| c["digest"].as_str())
                .filter(|d| crate::validate::valid_digest(d))
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

/// Remove a manifest and cascade over the index children it named (B1).
/// The bytes are read BEFORE the delete -- they are what names the
/// children. Returns how many child manifests were removed.
pub async fn delete_with_children(root: &Path, repo: &str, digest: &str) -> usize {
    let bytes = store::read_manifest(root, repo, digest)
        .await
        .unwrap_or_default();
    let _ = store::delete_manifest(root, repo, digest).await;
    prune_index_children(root, repo, &bytes).await
}

/// After an index was deleted, delete each child it named that no stored
/// manifest of the repo still lists and no tag points at; a child that is
/// itself an index cascades (bounded by [`MAX_INDEX_DEPTH`]). Returns how
/// many manifests were removed. Nothing here touches blobs: the caller's
/// `gc_blobs` pass reclaims them once unreferenced.
pub async fn prune_index_children(root: &Path, repo: &str, deleted: &[u8]) -> usize {
    let children = child_digests(deleted);
    if children.is_empty() {
        return 0; // image manifest (or unreadable): nothing hangs off it
    }
    // Reference counts over the SURVIVING manifests: when a child index is
    // removed its own references retire with it.
    let mut refs = child_ref_counts(root, repo).await;
    let tagged: HashSet<String> = store::list_tags(root, repo)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|(_, m)| m.digest)
        .collect();
    let mut pruned = 0;
    for child in children {
        pruned += prune_child(root, repo, &child, &tagged, &mut refs, 1).await;
    }
    pruned
}

/// `child digest -> number of stored manifests of the repo listing it`.
async fn child_ref_counts(root: &Path, repo: &str) -> HashMap<String, usize> {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for d in store::repo_manifest_digests(root, repo)
        .await
        .unwrap_or_default()
    {
        let Ok(bytes) = store::read_manifest(root, repo, &d).await else {
            continue;
        };
        for c in child_digests(&bytes) {
            *counts.entry(c).or_default() += 1;
        }
    }
    counts
}

/// One cascade step: drop `digest` when unreferenced and untagged, then
/// follow its own children one level deeper.
async fn prune_child(
    root: &Path,
    repo: &str,
    digest: &str,
    tagged: &HashSet<String>,
    refs: &mut HashMap<String, usize>,
    depth: usize,
) -> usize {
    if tagged.contains(digest) || refs.get(digest).copied().unwrap_or(0) > 0 {
        return 0; // still reachable: it and everything below it stays
    }
    let Ok(bytes) = store::read_manifest(root, repo, digest).await else {
        return 0; // never stored here (or already pruned)
    };
    let _ = store::delete_manifest(root, repo, digest).await;
    tracing::info!(target: "init-pro", repo, child = %digest, "registry: orphaned index child pruned");
    let grandchildren = child_digests(&bytes);
    if depth >= MAX_INDEX_DEPTH || grandchildren.is_empty() {
        return 1;
    }
    for g in &grandchildren {
        if let Some(n) = refs.get_mut(g) {
            *n = n.saturating_sub(1); // this manifest no longer pins it
        }
    }
    let mut pruned = 1;
    for g in grandchildren {
        pruned += Box::pin(prune_child(root, repo, &g, tagged, refs, depth + 1)).await;
    }
    pruned
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
    use serde_json::json;
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

    // ---- B1: index children ---------------------------------------------------

    const IMG_CT: &str = "application/vnd.docker.distribution.manifest.v2+json";

    fn meta(digest: &str, pushed_at: u64) -> store::TagMeta {
        store::TagMeta {
            digest: digest.to_string(),
            media_type: Some(IMG_CT.to_string()),
            pushed_at,
            seq: 0,
            size: 0,
        }
    }

    /// Store one image manifest (config + one layer blob in the CAS) under
    /// `repo`; returns its digest and the layer digest.
    async fn put_image(root: &Path, repo: &str, unique: &str) -> (String, String) {
        let cfg = format!("{{\"unique\":\"{unique}\"}}");
        let cfg_digest = store::sha256_hex(cfg.as_bytes());
        let layer = format!("layer-of-{unique}");
        let layer_digest = store::sha256_hex(layer.as_bytes());
        for (d, bytes) in [
            (cfg_digest.as_str(), cfg.as_bytes()),
            (layer_digest.as_str(), layer.as_bytes()),
        ] {
            tokio::fs::write(store::blob_path(root, d), bytes)
                .await
                .unwrap();
        }
        let manifest = format!(
            r#"{{"schemaVersion":2,"mediaType":"{IMG_CT}","config":{{"digest":"{cfg_digest}"}},"layers":[{{"digest":"{layer_digest}"}}]}}"#
        );
        let digest = store::sha256_hex(manifest.as_bytes());
        store::put_manifest(root, repo, &digest, manifest.as_bytes())
            .await
            .unwrap();
        (digest, layer_digest)
    }

    /// Store an index listing `children` under `repo`; returns its digest.
    async fn put_index(root: &Path, repo: &str, children: &[&str]) -> String {
        let body = serde_json::json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.docker.distribution.manifest.list.v2+json",
            "manifests": children.iter().map(|d| json!({"digest": d, "size": 1})).collect::<Vec<_>>(),
        });
        let manifest = serde_json::to_string(&body).unwrap();
        let digest = store::sha256_hex(manifest.as_bytes());
        store::put_manifest(root, repo, &digest, manifest.as_bytes())
            .await
            .unwrap();
        digest
    }

    /// B1: `child_digests` reads `manifests[].digest` only, and only
    /// grammatically valid digests (they become path segments).
    #[test]
    fn child_digests_reads_only_valid_index_entries() {
        let good = format!("sha256:{}", "a".repeat(64));
        let index = format!(
            r#"{{"manifests":[{{"digest":"{good}"}},{{"digest":"../../etc"}},{{"size":3}}]}}"#
        );
        assert_eq!(child_digests(index.as_bytes()), vec![good.clone()]);
        let image =
            format!(r#"{{"config":{{"digest":"{good}"}},"layers":[{{"digest":"{good}"}}]}}"#);
        assert!(child_digests(image.as_bytes()).is_empty());
        assert!(child_digests(b"not json").is_empty());
    }

    /// B1: deleting an index prunes the children nothing else reaches (one
    /// level down through a nested index) and, past the grace window, their
    /// blobs; the child a surviving index still lists stays.
    #[tokio::test]
    async fn deleting_an_index_prunes_orphaned_children_and_their_blobs() {
        let root = scratch("index-children").await;
        store::prepare(&root).unwrap();
        let (amd64, amd64_layer) = put_image(&root, "multi", "amd64").await;
        let (arm64, arm64_layer) = put_image(&root, "multi", "arm64").await;
        let (s390x, s390x_layer) = put_image(&root, "multi", "s390x").await;
        let nested = put_index(&root, "multi", &[&s390x]).await;
        let release = put_index(&root, "multi", &[&amd64, &arm64, &nested]).await;
        let shared = put_index(&root, "multi", &[&amd64]).await;

        assert_eq!(
            delete_with_children(&root, "multi", &release).await,
            3,
            "arm64 + the nested index + its child"
        );
        for gone in [&arm64, &nested, &s390x] {
            assert!(
                !store::manifest_exists(&root, "multi", gone).await,
                "{gone}"
            );
        }
        assert!(
            store::manifest_exists(&root, "multi", &amd64).await,
            "still listed by `shared`"
        );
        assert!(store::manifest_exists(&root, "multi", &shared).await);

        for d in [&amd64_layer, &arm64_layer, &s390x_layer] {
            age(&store::blob_path(&root, d), BLOB_GC_GRACE_SECS * 2);
        }
        gc_blobs(&root).await.unwrap();
        assert!(!store::blob_exists(&root, &arm64_layer).await);
        assert!(!store::blob_exists(&root, &s390x_layer).await);
        assert!(store::blob_exists(&root, &amd64_layer).await);

        assert_eq!(delete_with_children(&root, "multi", &shared).await, 1);
        assert!(!store::manifest_exists(&root, "multi", &amd64).await);
        let _ = tokio::fs::remove_dir_all(&root).await;
    }

    /// B1: a tag on the child keeps it when its parent index goes.
    #[tokio::test]
    async fn tagged_index_child_survives_its_parent() {
        let root = scratch("index-tagged").await;
        store::prepare(&root).unwrap();
        let (child, _) = put_image(&root, "multi", "amd64").await;
        store::write_tag(&root, "multi", "v1", &meta(&child, 1))
            .await
            .unwrap();
        let index = put_index(&root, "multi", &[&child]).await;
        assert_eq!(delete_with_children(&root, "multi", &index).await, 0);
        assert!(store::manifest_exists(&root, "multi", &child).await);
        let _ = tokio::fs::remove_dir_all(&root).await;
    }

    /// B1: the retention pass drops the index that lost its tag AND the
    /// digest-only child that lost its last parent.
    #[tokio::test]
    async fn retention_prunes_children_of_a_dropped_index() {
        let root = scratch("index-retention").await;
        store::prepare(&root).unwrap();
        let (a, _) = put_image(&root, "multi", "amd64").await;
        let (b, _) = put_image(&root, "multi", "arm64").await;
        let old = put_index(&root, "multi", &[&a]).await;
        let new = put_index(&root, "multi", &[&b]).await;
        for (tag, digest, at) in [("v1", &old, 1), ("v2", &new, 2)] {
            store::write_tag(&root, "multi", tag, &meta(digest, at))
                .await
                .unwrap();
        }
        apply_retention(&root, "multi", &[], 1).await.unwrap();
        assert!(!store::manifest_exists(&root, "multi", &old).await);
        assert!(
            !store::manifest_exists(&root, "multi", &a).await,
            "the dropped index's orphaned child must go"
        );
        assert!(store::manifest_exists(&root, "multi", &new).await);
        assert!(store::manifest_exists(&root, "multi", &b).await);
        let _ = tokio::fs::remove_dir_all(&root).await;
    }
}
