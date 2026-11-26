//! T4.13 image store (Q53): staging guard for `tmp/part-*` downloads.
//!
//! Every in-flight download is written to `tmp/part-<pid>-<n>-<nanos>` and
//! committed by an atomic rename into the CAS. Before C11 the temp file was
//! removed ONLY on the digest-mismatch arm, so an IO error, a timeout or a
//! panic left a partial blob behind forever (the M5 quota ignores `tmp/`, so
//! the leak was invisible to operators). Two faces fix that here:
//! [`PartGuard`] deletes the file on EVERY exit path (including panic
//! unwind) unless it was explicitly released for commit, and
//! [`sweep_part_files`] drops whatever a previous run left behind at
//! `ImageStore::open` time.

use std::path::{Path, PathBuf};

/// File-name prefix of one in-flight download (must match
/// [`super::cas::ImageStore::tmp_file`]).
pub const PART_PREFIX: &str = "part-";

/// RAII guard over one staged temp file: owns the path, deletes it on drop
/// while armed. `release` disarms and hands the path to the committer (the
/// rename moves the file, so there is nothing left to delete). Plain
/// `struct` + `impl` + `Drop` -- no classes, no `unsafe`.
#[derive(Debug)]
pub struct PartGuard {
    path: PathBuf,
    armed: bool,
}

impl PartGuard {
    /// Arm a guard for `path`. The file need not exist yet: dropping a
    /// guard whose file was never created is a no-op.
    pub fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    /// The staged path (create/write through this).
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Disarm and yield the path for the commit rename.
    pub fn release(mut self) -> PathBuf {
        self.armed = false;
        self.path.clone()
    }
}

impl Drop for PartGuard {
    fn drop(&mut self) {
        if self.armed {
            // Best effort: the file may never have been created, and a
            // failure here must not mask the error that caused the drop.
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Delete every `tmp/part-*` leftover from a previous run. Returns how many
/// files were removed; an unreadable `tmp/` dir is reported as `Err` so the
/// caller can log it without failing the store open.
pub fn sweep_part_files(tmp_dir: &Path) -> std::io::Result<usize> {
    let mut removed = 0usize;
    for entry in std::fs::read_dir(tmp_dir)?.flatten() {
        let is_part = entry
            .file_name()
            .to_str()
            .is_some_and(|n| n.starts_with(PART_PREFIX));
        if is_part {
            // Only plain files are touched: a hostile `part-*` directory or
            // symlink is left for the operator, never recursed into.
            if entry.file_type().is_ok_and(|t| t.is_file()) {
                std::fs::remove_file(entry.path())?;
                removed += 1;
            }
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "kubelet-part-{}-{}-{tag}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The `Err` arm of a download: the staged bytes must not survive.
    #[test]
    fn failure_path_leaves_no_part_file() {
        let dir = tmp("err");
        let guard = PartGuard::new(dir.join("part-1-0-1"));
        std::fs::write(guard.path(), b"half a blob").unwrap();
        drop(guard); // what a `return Err(...)` does
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Digest mismatch, IO error and timeout all funnel through the same
    /// drop; panic unwind must too (the guard is dropped while unwinding).
    #[test]
    fn panic_unwind_leaves_no_part_file() {
        let dir = tmp("panic");
        let path = dir.join("part-2-0-2");
        let arg = path.clone();
        let boom = std::panic::catch_unwind(move || {
            let guard = PartGuard::new(arg);
            std::fs::write(guard.path(), b"partial").unwrap();
            panic!("simulated digest failure");
        });
        assert!(boom.is_err(), "the closure must unwind");
        assert!(!path.exists(), "unwind dropped the guard");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The success path keeps the bytes: `release` hands them to `commit`.
    #[test]
    fn release_disarms_the_guard() {
        let dir = tmp("release");
        let guard = PartGuard::new(dir.join("part-3-0-3"));
        std::fs::write(guard.path(), b"whole blob").unwrap();
        let committed = guard.release();
        assert_eq!(std::fs::read(&committed).unwrap(), b"whole blob");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A guard for a file that was never created is a no-op drop.
    #[test]
    fn missing_file_drop_is_a_noop() {
        let dir = tmp("missing");
        drop(PartGuard::new(dir.join("part-4-0-4")));
        assert!(dir.is_dir());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sweep_removes_only_part_files() {
        let dir = tmp("sweep");
        std::fs::write(dir.join("part-9-9-9"), b"stale").unwrap();
        std::fs::write(dir.join("other.txt"), b"keep").unwrap();
        std::fs::create_dir_all(dir.join("part-dir")).unwrap();
        std::os::unix::fs::symlink(dir.join("other.txt"), dir.join("part-link")).unwrap();
        assert_eq!(sweep_part_files(&dir).unwrap(), 1);
        assert!(!dir.join("part-9-9-9").exists());
        assert!(dir.join("other.txt").is_file(), "non-part file kept");
        assert!(dir.join("part-dir").is_dir(), "dirs are never recursed");
        assert!(std::fs::symlink_metadata(dir.join("part-link")).is_ok());
        assert_eq!(sweep_part_files(&dir).unwrap(), 0, "idempotent");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
