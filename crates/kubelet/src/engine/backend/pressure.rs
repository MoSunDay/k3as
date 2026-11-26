//! C3 (bug sweep): the disk-pressure-aware GC pass over the T4.13 image
//! store. [`super::EngineBackend::image_gc`] is the only caller -- the
//! kubelet sync loop calls it once per cycle with the images its desired
//! pod set still needs (`crate::image_gc`).
//!
//! Split out of `backend.rs` at the 800-line rule. Every decision is pure
//! and lives in [`crate::engine::image::disk`]; this module is the I/O
//! shell around it:
//!
//! 1. measure the image root filesystem with `nix::sys::statvfs::statvfs`
//!    (a safe wrapper -- no raw syscalls in this crate);
//! 2. `disk::classify` the reading; `Healthy` returns without touching the
//!    store (one statvfs per sync cycle, nothing else);
//! 3. Critical: drop the oldest pull records nothing needs (stage A);
//! 4. delete the orphan blobs / per-layer dirs / fused dirs (stage B), read
//!    AFTER stage A on purpose so a dropped ref's unique blobs go in the
//!    same pass;
//! 5. log ONE summary line (level, bytes freed, entries removed).
//!
//! Every deletion is best-effort: a failed unlink is a `warn!`, never a
//! failed reconcile. `views/` (live per-container overlay scratch) and
//! `tmp/` (in-flight downloads) are never walked here.

use std::collections::BTreeSet;
use std::path::Path;
use std::time::SystemTime;

use nix::sys::statvfs::statvfs;

use crate::cri_backend::ImageGcStats;
use crate::engine::image::disk::{self, DiskPressure, Entry, InUse, RefEntry};
use crate::engine::image::ImageStore;

use super::EngineBackend;

/// One pass over `backend`'s store: measure, decide, sweep best-effort.
/// Never fails and never panics on I/O -- the answer is in the stats.
pub(super) fn pass(backend: &EngineBackend, in_use: &[String]) -> ImageGcStats {
    let (available, total) = measure(backend.store.root());
    pass_at(backend, in_use, available, total, SystemTime::now())
}

/// The deterministic core of [`pass`]: the measurement and the clock are
/// arguments, so the tests can drive a pressured root without filling a
/// disk. Everything else (store walks, deletion, logging) is the real path.
pub(super) fn pass_at(
    backend: &EngineBackend,
    in_use: &[String],
    available_bytes: u64,
    total_bytes: u64,
    now: SystemTime,
) -> ImageGcStats {
    let pressure = disk::classify(available_bytes, total_bytes);
    let store = &backend.store;
    if pressure == DiskPressure::Healthy {
        tracing::debug!(
            target: "init-pro",
            available_bytes,
            total_bytes,
            "image GC: image root has space (C3)"
        );
        return ImageGcStats {
            pressure,
            ..Default::default()
        };
    }

    let images: Vec<String> = backend
        .registry
        .images()
        .into_iter()
        .map(|(_, image)| image)
        .chain(std::iter::once(backend.sandbox_image.clone()))
        .chain(in_use.iter().cloned())
        .collect();
    let in_use = InUse::new(images.iter().map(String::as_str), backend.puller.host());
    let before = store.usage_bytes();
    let mut stats = ImageGcStats {
        pressure,
        ..Default::default()
    };

    // Stage A (Critical only): the oldest pull records no live pod,
    // container or sandbox needs. Dropping the ref FILE is the unit of
    // ownership (T4.13 M5): the blobs it alone owned become orphans and
    // stage B frees them in this same pass. Shared layers stay marked by
    // every surviving ref, so dedupe is untouched.
    let ref_plan = disk::plan(pressure, &[], &ref_entries(store), &in_use, now);
    for reference in &ref_plan.refs {
        if store.remove_ref(reference) {
            stats.refs_removed += 1;
        } else {
            tracing::warn!(
                target: "init-pro",
                reference = %reference,
                "image GC: could not drop pull record (continuing)"
            );
        }
    }

    // Stage B: orphans -- digests no surviving ref owns, older than the
    // grace age. The grace is what keeps an in-flight pull safe: blobs and
    // `layers/` dirs are written BEFORE their ref file exists.
    let orphan_plan = disk::plan(pressure, &orphan_entries(store), &[], &in_use, now);
    for digest in &orphan_plan.orphans {
        stats.entries_removed += remove_digest(store, digest);
    }

    stats.bytes_freed = before.saturating_sub(store.usage_bytes());
    log_summary(pressure, &stats, available_bytes, total_bytes, in_use.len());
    stats
}

/// ONE summary line per pass (C3): the level, the bytes freed and the
/// entries removed. `info` when the pass actually freed something,
/// `debug` when nothing was deletable -- a disk that stays full must not
/// spam the log every sync cycle.
fn log_summary(
    pressure: DiskPressure,
    stats: &ImageGcStats,
    available_bytes: u64,
    total_bytes: u64,
    in_use: usize,
) {
    let line = "builtin image GC: disk pressure pass (C3)";
    if stats.entries_removed + stats.refs_removed == 0 {
        tracing::debug!(
            target: "init-pro",
            level = pressure.as_str(),
            available_bytes,
            total_bytes,
            freed = stats.bytes_freed,
            entries = stats.entries_removed,
            refs = stats.refs_removed,
            in_use,
            "{line}"
        );
    } else {
        tracing::info!(
            target: "init-pro",
            level = pressure.as_str(),
            available_bytes,
            total_bytes,
            freed = stats.bytes_freed,
            entries = stats.entries_removed,
            refs = stats.refs_removed,
            in_use,
            "{line}"
        );
    }
}

/// `(available_bytes, total_bytes)` of the filesystem holding `root`.
/// `statvfs` blocks are in `fragment_size()` units. An unreadable root
/// reads `(0, 0)`, which `disk::classify` maps to `Healthy`: an
/// unmeasurable store must never trigger a blind sweep.
fn measure(root: &Path) -> (u64, u64) {
    match statvfs(root) {
        Ok(vfs) => {
            // Blocks are in `fragment_size()` units; both are u64 here.
            let fragment = vfs.fragment_size().max(1);
            (
                vfs.blocks_available().saturating_mul(fragment),
                vfs.blocks().saturating_mul(fragment),
            )
        }
        Err(e) => {
            tracing::warn!(
                target: "init-pro",
                root = %root.display(),
                error = %e,
                "image GC: statvfs failed; skipping the pass (C3)"
            );
            (0, 0)
        }
    }
}

/// Every pull record with the mtime of its ref file.
fn ref_entries(store: &ImageStore) -> Vec<RefEntry> {
    store
        .list_refs()
        .into_iter()
        .map(|img| {
            let modified = mtime(&ref_path(store, &img.reference));
            RefEntry {
                reference: img.reference,
                digest: img.digest,
                modified,
            }
        })
        .collect()
}

/// The ref-file path of `reference` (same key rule as the store: `/`,
/// `:` and `@` become `_`).
fn ref_path(store: &ImageStore, reference: &str) -> std::path::PathBuf {
    let key = reference.replace(['/', ':', '@'], "_").replace('\\', "_");
    store.root().join("refs").join(format!("{key}.json"))
}

/// Every on-disk digest no surviving ref owns, with its mtime: CAS blobs
/// (`blobs/<algo>/<hex>`), unpacked per-layer dirs and fused `merged/`
/// dirs (the last two are keyed by bare hex, so they read `sha256:<hex>`,
/// exactly like `gc::sweep` marks them).
fn orphan_entries(store: &ImageStore) -> Vec<Entry> {
    let mut marked = BTreeSet::new();
    for img in store.list_refs() {
        marked.insert(img.digest);
        marked.insert(img.config.digest);
        for layer in img.layers {
            marked.insert(layer.digest);
        }
    }
    let root = store.root();
    let mut out = Vec::new();
    for algo in sub_dirs(&root.join("blobs")) {
        let dir = root.join("blobs").join(&algo);
        for hex in sub_files(&dir) {
            let digest = format!("{algo}:{hex}");
            if !marked.contains(&digest) {
                out.push(Entry {
                    key: digest,
                    modified: mtime(&dir.join(&hex)),
                });
            }
        }
    }
    for sub in ["layers", "merged"] {
        for hex in sub_dirs(&root.join(sub)) {
            let digest = format!("sha256:{hex}");
            if !marked.contains(&digest) {
                out.push(Entry {
                    key: digest,
                    modified: mtime(&root.join(sub).join(&hex)),
                });
            }
        }
    }
    out
}

/// Delete every on-disk face of one digest (blob + layer dir + merged
/// dir). Returns how many faces went; each failure is a warn, never an
/// error -- the next pass retries whatever is still there.
fn remove_digest(store: &ImageStore, digest: &str) -> usize {
    let mut removed = 0;
    let blob = store.blob_path(digest);
    if blob.is_file() {
        match std::fs::remove_file(&blob) {
            Ok(()) => removed += 1,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!(
                target: "init-pro",
                path = %blob.display(),
                error = %e,
                "image GC: could not remove blob (continuing)"
            ),
        }
    }
    for dir in [store.layer_dir(digest), store.merged_dir(digest)] {
        if !dir.is_dir() {
            continue;
        }
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => removed += 1,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!(
                target: "init-pro",
                path = %dir.display(),
                error = %e,
                "image GC: could not remove dir (continuing)"
            ),
        }
    }
    removed
}

/// mtime of one path; `UNIX_EPOCH` when it cannot be read (such an entry
/// reads as aged, and its deletion then fails loudly instead of silently).
fn mtime(path: &Path) -> SystemTime {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

/// Immediate subdirectory names (empty on any I/O error).
fn sub_dirs(dir: &Path) -> Vec<String> {
    entries(dir).0
}

/// Immediate regular-file names (empty on any I/O error).
fn sub_files(dir: &Path) -> Vec<String> {
    entries(dir).1
}

/// One `read_dir`, split into `(dirs, files)` names.
fn entries(dir: &Path) -> (Vec<String>, Vec<String>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return (Vec::new(), Vec::new());
    };
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    for entry in rd.flatten() {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if entry.path().is_dir() {
            dirs.push(name);
        } else if entry.path().is_file() {
            files.push(name);
        }
    }
    (dirs, files)
}

#[cfg(test)]
// C3 pressure-pass cases; split out at the 400-line rule.
mod tests;
