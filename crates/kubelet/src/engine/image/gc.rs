//! T4.13 image store (Q53/Q54): mark-sweep GC over the CAS layout.
//! Split from `cas.rs` at M3 (file-size rule) — pure filesystem walks
//! over the store root, nothing else.
//!
//! D: in-flight staging dirs (`layers/.tmp-<hex>-<pid>`, `merged/.tmp-*`,
//! see `layers.rs`) are invisible to the sweep — an unpack publishes by
//! rename, so only digest-named dirs are ever GC candidates — and the
//! async callers reach the sweep through [`gc_offload`] (blocking pool).

use std::path::Path;

use super::cas::{ImageStore, PulledImage};
use super::layers::is_staging_name;

/// Mark-sweep GC (Q53): mark the manifest/config/layer digests of every
/// ref, then delete CAS blobs, `merged/` fused dirs and `layers/`
/// per-layer dirs no ref still owns. `views/` is ephemeral per-container
/// overlay state and is never swept here. Only ever walks inside the
/// store root. Returns `(blobs_removed, merged_removed, layers_removed)`.
pub(crate) fn sweep(root: &Path, refs: &[PulledImage]) -> Result<(usize, usize, usize), String> {
    let mut marked: Vec<String> = Vec::new();
    for img in refs {
        marked.push(img.digest.clone());
        marked.push(img.config.digest.clone());
        marked.extend(img.layers.iter().map(|l| l.digest.clone()));
    }
    let is_marked = |d: &str| marked.iter().any(|m| m == d);

    let mut blobs_removed = 0usize;
    let blobs_root = root.join("blobs");
    for algo in list_dirs(&blobs_root) {
        for hex in list_files(&blobs_root.join(&algo)) {
            if !is_marked(&format!("{algo}:{hex}")) {
                let p = blobs_root.join(&algo).join(&hex);
                std::fs::remove_file(&p).map_err(|e| format!("gc blob {}: {e}", p.display()))?;
                blobs_removed += 1;
            }
        }
    }

    let mut merged_removed = 0usize;
    for hex in list_dirs(&root.join("merged")) {
        if !is_marked(&format!("sha256:{hex}")) {
            let p = root.join("merged").join(&hex);
            std::fs::remove_dir_all(&p).map_err(|e| format!("gc merged {}: {e}", p.display()))?;
            merged_removed += 1;
        }
    }

    // T4.13 M3: unreferenced per-layer dirs are swept with the blobs
    // that fed them; a layer shared by ANY ref stays (dedupe by digest).
    let mut layers_removed = 0usize;
    for hex in list_dirs(&root.join("layers")) {
        if !is_marked(&format!("sha256:{hex}")) {
            let p = root.join("layers").join(&hex);
            std::fs::remove_dir_all(&p).map_err(|e| format!("gc layer {}: {e}", p.display()))?;
            layers_removed += 1;
        }
    }
    Ok((blobs_removed, merged_removed, layers_removed))
}

/// [`ImageStore::gc`] off the async path (D): the sweep walks and unlinks,
/// so the async callers (`recover`, `remove_image`) run it on the blocking
/// pool instead of a tokio worker. A join failure is reported, never lost.
pub async fn gc_offload(store: ImageStore) -> Result<(usize, usize, usize), String> {
    tokio::task::spawn_blocking(move || store.gc())
        .await
        .unwrap_or_else(|e| Err(format!("builtin image GC task failed: {e}")))
}

/// Immediate subdirectory names, in-flight staging dirs excluded (D).
fn list_dirs(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.path().is_dir())
                .filter(|e| !is_staging_name(&e.file_name().to_string_lossy()))
                .filter_map(|e| e.file_name().into_string().ok())
                .collect()
        })
        .unwrap_or_default()
}

/// Immediate regular-file names (empty on any I/O error).
fn list_files(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.path().is_file())
                .filter_map(|e| e.file_name().into_string().ok())
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> std::path::PathBuf {
        let root =
            std::env::temp_dir().join(format!("init-pro-image-gc-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        root
    }

    /// D: a sweep racing an in-flight unpack leaves the staging dirs alone
    /// while unreferenced published dirs still go in the same pass.
    #[test]
    fn sweep_skips_in_flight_staging_dirs() {
        let root = temp_root("staging");
        let store = ImageStore::open(&root).unwrap();
        let layer_staging = root.join("layers").join(".tmp-a-1");
        let merged_staging = root
            .join("merged")
            .join(format!(".tmp-b-{}", std::process::id()));
        std::fs::create_dir_all(layer_staging.join("bin")).unwrap();
        std::fs::create_dir_all(&merged_staging).unwrap();
        std::fs::create_dir_all(root.join("layers").join("c".repeat(64))).unwrap();
        std::fs::create_dir_all(root.join("merged").join("d".repeat(64))).unwrap();
        assert_eq!(sweep(&root, &[]).unwrap(), (0, 1, 1));
        assert!(layer_staging.join("bin").exists(), "layer staging swept");
        assert!(merged_staging.exists(), "merged staging swept");
        assert!(!root.join("layers").join("c".repeat(64)).exists());
        assert!(!root.join("merged").join("d".repeat(64)).exists());
        assert!(is_staging_name(".tmp-abc-1"));
        assert!(!is_staging_name("abc"));
        assert_eq!(store.gc().unwrap(), (0, 0, 0), "second pass is a no-op");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// D: the offloaded sweep is the same sweep, on the blocking pool.
    #[tokio::test]
    async fn gc_offload_runs_the_sync_sweep() {
        let root = temp_root("offload");
        let store = ImageStore::open(&root).unwrap();
        std::fs::create_dir_all(root.join("layers").join("e".repeat(64))).unwrap();
        assert_eq!(gc_offload(store).await.unwrap(), (0, 0, 1));
        assert_eq!(sweep(&root, &[]).unwrap(), (0, 0, 0), "the dir is gone");
        let _ = std::fs::remove_dir_all(&root);
    }
}
