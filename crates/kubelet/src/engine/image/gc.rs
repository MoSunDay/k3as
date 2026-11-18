//! T4.13 image store (Q53/Q54): mark-sweep GC over the CAS layout.
//! Split from `cas.rs` at M3 (file-size rule) — pure filesystem walks
//! over the store root, nothing else.

use std::path::Path;

use super::cas::PulledImage;

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

/// Immediate subdirectory names (empty on any I/O error).
fn list_dirs(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.path().is_dir())
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
