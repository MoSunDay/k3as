//! Scratch sweeper for the pipeline plane (TODO **T8.3**, Q56; batch D / B4).
//!
//! `build::reconcile` writes `<scratch>/<ns>/<build>/image.tar` (up to 8 GiB)
//! and the importer stages layers under `<scratch>/import/`; both are only
//! removed on the in-process exits, so a panic, a leader loss or a SIGKILL
//! mid `docker save` leaked them forever (empty `<ns>/` dirs too). [`sweep`]
//! reclaims whatever is older than [`SCRATCH_TTL`] (age is a safe liveness
//! probe: no live Build runs for a day); [`sweep_due`] spaces passes
//! [`SWEEP_INTERVAL`] apart and rides the ServicePipeline reconcile (the
//! worker's 2s resync; the loop state lives in `runner_worker.rs`), so the
//! first pass after boot is the start-up sweep. Never errors: a failed
//! removal is logged and left to the next pass.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[cfg(test)]
mod tests;

/// Older than this, a per-build dir or an import staging dir is garbage.
pub const SCRATCH_TTL: Duration = Duration::from_secs(24 * 60 * 60);
/// Minimum spacing between two sweeps of the scratch root.
pub const SWEEP_INTERVAL: Duration = Duration::from_secs(10 * 60);
const SWEEP_INTERVAL_MILLIS: u64 = SWEEP_INTERVAL.as_secs() * 1000;
/// Importer staging root under the scratch dir (T6.3 `tarstream::staging_dir`).
const IMPORT_DIR: &str = "import";
/// Wall-clock millis of the last sweep in this process (0 = none yet).
static LAST_SWEEP_MILLIS: AtomicU64 = AtomicU64::new(0);

/// A dated directory (path, mtime).
type Entry = (PathBuf, SystemTime);
fn millis(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Pure cadence rule: a sweep is due when none ran yet or the last one is
/// at least [`SWEEP_INTERVAL`] old (a clock that went backwards defers).
pub fn sweep_is_due(last_millis: u64, now_millis: u64) -> bool {
    last_millis == 0 || now_millis.saturating_sub(last_millis) >= SWEEP_INTERVAL_MILLIS
}

/// Pure: is an entry dated `mtime` older than `ttl` at `now`? A future-
/// dated entry (clock skew) never is -- never delete on doubt.
pub fn expired(now: SystemTime, mtime: SystemTime, ttl: Duration) -> bool {
    now.duration_since(mtime)
        .map(|age| age > ttl)
        .unwrap_or(false)
}

/// Pure: the entries older than `ttl` ([`expired`]).
pub fn sweep_plan(entries: Vec<Entry>, now: SystemTime, ttl: Duration) -> Vec<PathBuf> {
    entries
        .into_iter()
        .filter(|(_, mtime)| expired(now, *mtime, ttl))
        .map(|(path, _)| path)
        .collect()
}

/// [`sweep`] when due; at most one pass per [`SWEEP_INTERVAL`] per process.
pub async fn sweep_due(root: &Path) -> usize {
    let now = SystemTime::now();
    let last = LAST_SWEEP_MILLIS.load(Ordering::Relaxed);
    let due = sweep_is_due(last, millis(now))
        && LAST_SWEEP_MILLIS
            .compare_exchange(last, millis(now), Ordering::Relaxed, Ordering::Relaxed)
            .is_ok();
    if due {
        sweep(root, SCRATCH_TTL, now).await
    } else {
        0
    }
}

/// Remove every per-build dir, import staging dir and EMPTY namespace dir
/// under `root` that is older than `ttl`; returns how many were removed.
pub async fn sweep(root: &Path, ttl: Duration, now: SystemTime) -> usize {
    let (garbage, empty_namespaces) = scan(root).await;
    let mut removed = 0;
    for path in sweep_plan(garbage, now, ttl) {
        removed += remove_tree(&path).await as usize;
    }
    // `remove_dir` never recurses: a namespace that received a new Build
    // since the listing is left alone for a later pass.
    for path in sweep_plan(empty_namespaces, now, ttl) {
        if tokio::fs::remove_dir(&path).await.is_ok() {
            warn_swept(&path);
            removed += 1;
        }
    }
    removed
}

/// One listing pass (never deeper than `<root>/<a>/<b>`): per-build and
/// import staging dirs as garbage, plus the namespace dirs empty right now.
async fn scan(root: &Path) -> (Vec<Entry>, Vec<Entry>) {
    let mut garbage = Vec::new();
    let mut empty_namespaces = Vec::new();
    for (dir, mtime) in subdirs(root).await {
        let is_import = dir.file_name().is_some_and(|n| n == IMPORT_DIR);
        let kids = subdirs(&dir).await;
        match (is_import, kids.is_empty()) {
            (true, _) => garbage.extend(kids),
            (false, true) => empty_namespaces.push((dir, mtime)),
            (false, false) => garbage.extend(kids),
        }
    }
    (garbage, empty_namespaces)
}

/// Real subdirectories of `dir` one level down with their mtimes; symlinks
/// and un-datable entries are skipped (never delete on doubt).
async fn subdirs(dir: &Path) -> Vec<Entry> {
    let Ok(mut rd) = tokio::fs::read_dir(dir).await else {
        return Vec::new(); // no scratch root: nothing to reclaim
    };
    let mut out = Vec::new();
    while let Ok(Some(e)) = rd.next_entry().await {
        // DirEntry::metadata does not follow symlinks: a link is no dir.
        let Ok(meta) = e.metadata().await else {
            continue;
        };
        if let (true, Ok(mtime)) = (meta.is_dir(), meta.modified()) {
            out.push((e.path(), mtime));
        }
    }
    out
}

/// Best-effort recursive removal (a plain file is unlinked instead).
async fn remove_tree(path: &Path) -> bool {
    let is_dir = tokio::fs::symlink_metadata(path)
        .await
        .map(|m| m.is_dir())
        .unwrap_or(false);
    let result = if is_dir {
        tokio::fs::remove_dir_all(path).await
    } else {
        tokio::fs::remove_file(path).await
    };
    if result.is_err() {
        return false;
    }
    warn_swept(path);
    true
}

fn warn_swept(path: &Path) {
    tracing::warn!(target: "init-pro", path = %path.display(), "pipeline: stale scratch swept");
}
