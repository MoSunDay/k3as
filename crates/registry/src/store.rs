//! Filesystem layout + primitives for the embedded registry (T6.3, Q40).
//!
//! Pure fs mechanics only: path encoding, CAS blob writes, manifest and
//! tag records, directory walks, and file hashing. Retention/GC
//! orchestration lives in gc.rs; policy math in policy.rs.

use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use tokio::io::AsyncReadExt;

/// Tag record persisted at `repos/<name>/tags/<tag>.json`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TagMeta {
    pub digest: String,
    /// Content-Type the manifest was pushed with (replayed on GET).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
    /// Unix-epoch millis of the push; retention ordering key.
    pub pushed_at: u64,
    /// Monotonic tie-breaker for same-millis pushes.
    #[serde(default)]
    pub seq: u64,
    pub size: u64,
}

// ---- paths ----------------------------------------------------------------

pub fn root_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("registry")
}

/// Callers must validate `digest` with `validate::valid_digest` first;
/// never build a path from client input directly.
pub fn blob_path(root: &Path, digest: &str) -> PathBuf {
    debug_assert!(
        crate::validate::valid_digest(digest),
        "invalid digest passed to blob_path"
    );
    root.join("blobs").join(encode_digest(digest))
}

pub fn repo_dir(root: &Path, name: &str) -> PathBuf {
    root.join("repos").join(name)
}

/// Callers must validate `digest` with `validate::valid_digest` first;
/// never build a path from client input directly.
pub fn manifest_path(root: &Path, name: &str, digest: &str) -> PathBuf {
    debug_assert!(
        crate::validate::valid_digest(digest),
        "invalid digest passed to manifest_path"
    );
    repo_dir(root, name)
        .join("manifests")
        .join(encode_digest(digest))
}

pub fn tag_path(root: &Path, name: &str, tag: &str) -> PathBuf {
    repo_dir(root, name)
        .join("tags")
        .join(format!("{tag}.json"))
}

fn upload_path(root: &Path, uuid: &str) -> PathBuf {
    root.join("uploads").join(uuid)
}

/// `sha256:<hex>` -> filesystem-safe `sha256-<hex>`.
pub fn encode_digest(digest: &str) -> String {
    digest.replace(':', "-")
}

fn decode_digest(file: &str) -> Option<String> {
    let (algo, hex) = file.split_once('-')?;
    Some(format!("{algo}:{hex}"))
}

/// Self-heal the upload staging dir (a wiped data-dir mid-flight must
/// not wedge pushes; boot-time `prepare` covers the normal path).
pub async fn ensure_uploads(root: &Path) -> io::Result<()> {
    tokio::fs::create_dir_all(root.join("uploads")).await
}

pub fn prepare(root: &Path) -> io::Result<()> {
    for d in ["blobs", "repos", "uploads"] {
        std::fs::create_dir_all(root.join(d))?;
    }
    Ok(())
}

// ---- digests --------------------------------------------------------------

pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    let out = h.finalize();
    let mut s = String::with_capacity(71);
    s.push_str("sha256:");
    for b in out {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Stream-hash a file in 64 KiB chunks (blobs never fully buffered).
pub async fn hash_file(path: &Path) -> io::Result<String> {
    let mut f = tokio::fs::File::open(path).await?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    let out = h.finalize();
    let mut s = String::from("sha256:");
    for b in out {
        s.push_str(&format!("{b:02x}"));
    }
    Ok(s)
}

// ---- blobs ----------------------------------------------------------------

pub async fn blob_exists(root: &Path, digest: &str) -> bool {
    tokio::fs::metadata(blob_path(root, digest)).await.is_ok()
}

pub async fn blob_len(root: &Path, digest: &str) -> io::Result<u64> {
    Ok(tokio::fs::metadata(blob_path(root, digest)).await?.len())
}

pub async fn read_blob(root: &Path, digest: &str) -> io::Result<Vec<u8>> {
    tokio::fs::read(blob_path(root, digest)).await
}

/// Move a fully-written staging file into the global blob CAS. An
/// already-present digest is a no-op (content-addressed idempotence).
pub async fn commit_blob(root: &Path, staged: &Path, digest: &str) -> io::Result<()> {
    let dest = blob_path(root, digest);
    if let Some(dir) = dest.parent() {
        tokio::fs::create_dir_all(dir).await?;
    }
    if tokio::fs::metadata(&dest).await.is_ok() {
        let _ = tokio::fs::remove_file(staged).await;
        return Ok(());
    }
    tokio::fs::rename(staged, dest).await
}

pub async fn delete_blob(root: &Path, digest: &str) -> io::Result<()> {
    let _ = tokio::fs::remove_file(blob_path(root, digest)).await;
    Ok(())
}

// ---- manifests ------------------------------------------------------------

pub async fn manifest_exists(root: &Path, name: &str, digest: &str) -> bool {
    tokio::fs::metadata(manifest_path(root, name, digest))
        .await
        .is_ok()
}

pub async fn put_manifest(root: &Path, name: &str, digest: &str, bytes: &[u8]) -> io::Result<()> {
    let p = manifest_path(root, name, digest);
    if let Some(dir) = p.parent() {
        tokio::fs::create_dir_all(dir).await?;
    }
    tokio::fs::write(p, bytes).await
}

pub async fn read_manifest(root: &Path, name: &str, digest: &str) -> io::Result<Vec<u8>> {
    tokio::fs::read(manifest_path(root, name, digest)).await
}

pub async fn delete_manifest(root: &Path, name: &str, digest: &str) -> io::Result<()> {
    let _ = tokio::fs::remove_file(manifest_path(root, name, digest)).await;
    Ok(())
}

/// Digests of all manifests currently stored for one repository.
pub async fn repo_manifest_digests(root: &Path, name: &str) -> io::Result<Vec<String>> {
    let dir = repo_dir(root, name).join("manifests");
    let mut out = Vec::new();
    let mut rd = match tokio::fs::read_dir(&dir).await {
        Ok(rd) => rd,
        Err(_) => return Ok(out),
    };
    while let Some(e) = rd.next_entry().await? {
        if let Some(f) = e.file_name().to_str() {
            if let Some(d) = decode_digest(f) {
                out.push(d);
            }
        }
    }
    Ok(out)
}

// ---- tags -----------------------------------------------------------------

pub async fn read_tag(root: &Path, name: &str, tag: &str) -> io::Result<TagMeta> {
    let raw = tokio::fs::read(tag_path(root, name, tag)).await?;
    Ok(serde_json::from_slice(&raw)?)
}

pub async fn write_tag(root: &Path, name: &str, tag: &str, meta: &TagMeta) -> io::Result<()> {
    let p = tag_path(root, name, tag);
    if let Some(dir) = p.parent() {
        tokio::fs::create_dir_all(dir).await?;
    }
    tokio::fs::write(p, serde_json::to_vec_pretty(meta)?).await
}

pub async fn delete_tag(root: &Path, name: &str, tag: &str) -> io::Result<()> {
    let _ = tokio::fs::remove_file(tag_path(root, name, tag)).await;
    Ok(())
}

/// All tags of one repository with their records, tag-sorted.
pub async fn list_tags(root: &Path, name: &str) -> io::Result<Vec<(String, TagMeta)>> {
    let dir = repo_dir(root, name).join("tags");
    let mut files = Vec::new();
    let mut rd = match tokio::fs::read_dir(&dir).await {
        Ok(rd) => rd,
        Err(_) => return Ok(Vec::new()),
    };
    while let Some(e) = rd.next_entry().await? {
        let f = e.file_name();
        if let Some(t) = f.to_str().and_then(|s| s.strip_suffix(".json")) {
            files.push(t.to_string());
        }
    }
    files.sort();
    let mut out = Vec::with_capacity(files.len());
    for t in files {
        if let Ok(m) = read_tag(root, name, &t).await {
            out.push((t, m));
        }
    }
    Ok(out)
}

pub async fn repo_exists(root: &Path, name: &str) -> bool {
    let dir = repo_dir(root, name);
    tokio::fs::metadata(dir.join("tags")).await.is_ok()
        || tokio::fs::metadata(dir.join("manifests")).await.is_ok()
}

/// All repository names (a directory is a repo iff it carries a `tags`
/// or `manifests` subdir; nesting `a` and `a/b` both works).
pub async fn list_repos(root: &Path) -> io::Result<Vec<String>> {
    let mut out = Vec::new();
    walk_repos(&root.join("repos"), String::new(), &mut out).await;
    out.sort();
    Ok(out)
}

async fn walk_repos(dir: &Path, prefix: String, out: &mut Vec<String>) {
    let rd = match tokio::fs::read_dir(dir).await {
        Ok(rd) => rd,
        Err(_) => return,
    };
    let mut has_repo = false;
    let mut subdirs = Vec::new();
    let mut entries = Vec::new();
    let mut rd = rd;
    while let Ok(Some(e)) = rd.next_entry().await {
        entries.push(e);
    }
    for e in entries {
        let fname = e.file_name();
        let f = match fname.to_str() {
            Some(f) => f,
            None => continue,
        };
        if f == "tags" || f == "manifests" {
            has_repo = true;
        } else if e.path().is_dir() {
            subdirs.push((f.to_string(), e.path()));
        }
    }
    if has_repo && !prefix.is_empty() {
        out.push(prefix.clone());
    }
    for (f, p) in subdirs {
        let next = if prefix.is_empty() {
            f
        } else {
            format!("{prefix}/{f}")
        };
        Box::pin(walk_repos(&p, next, out)).await;
    }
}

/// Blob digests referenced by every stored manifest across all repos
/// (`config.digest` + `layers[].digest`). Index children point at
/// manifests, which die with their last tag — not blobs.
pub async fn referenced_blob_digests(root: &Path) -> HashSet<String> {
    let mut refs = HashSet::new();
    let Ok(repos) = list_repos(root).await else {
        return refs;
    };
    for name in repos {
        for d in repo_manifest_digests(root, &name).await.unwrap_or_default() {
            let Ok(bytes) = read_manifest(root, &name, &d).await else {
                continue;
            };
            let Ok(v) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
                continue;
            };
            if let Some(c) = v["config"]["digest"].as_str() {
                refs.insert(c.to_string());
            }
            if let Some(layers) = v["layers"].as_array() {
                for l in layers {
                    if let Some(x) = l["digest"].as_str() {
                        refs.insert(x.to_string());
                    }
                }
            }
        }
    }
    refs
}

/// Delete blob files referenced by no stored manifest. `grace_secs`
/// protects blobs whose manifest push may still be in flight (the
/// upload and the manifest PUT are two client round-trips).
pub async fn gc_blobs(root: &Path, grace_secs: u64, now_unix: u64) -> io::Result<usize> {
    let refs = referenced_blob_digests(root).await;
    let dir = root.join("blobs");
    let mut rd = match tokio::fs::read_dir(&dir).await {
        Ok(rd) => rd,
        Err(_) => return Ok(0),
    };
    let mut removed = 0;
    let mut entries = Vec::new();
    while let Ok(Some(e)) = rd.next_entry().await {
        entries.push(e);
    }
    for e in entries {
        let Some(f) = e.file_name().to_str().and_then(decode_digest) else {
            continue;
        };
        if refs.contains(&f) {
            continue;
        }
        let stale = tokio::fs::metadata(e.path())
            .await
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| now_unix.saturating_sub(d.as_secs()) > grace_secs)
            .unwrap_or(true);
        if stale && tokio::fs::remove_file(e.path()).await.is_ok() {
            removed += 1;
        }
    }
    Ok(removed)
}

// ---- uploads --------------------------------------------------------------

pub fn upload_file(root: &Path, uuid: &str) -> PathBuf {
    upload_path(root, uuid)
}

pub async fn upload_exists(root: &Path, uuid: &str) -> bool {
    tokio::fs::metadata(upload_path(root, uuid)).await.is_ok()
}

pub async fn delete_upload(root: &Path, uuid: &str) -> io::Result<()> {
    let _ = tokio::fs::remove_file(upload_path(root, uuid)).await;
    Ok(())
}
