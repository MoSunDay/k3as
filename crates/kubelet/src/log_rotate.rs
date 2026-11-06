//! Pod log layout + copytruncate rotation (TODO **T4.6**, decision **Q39**).
//!
//! Container log files are written and fd-held by the vendored containerd
//! through the CRI `log_directory`/`log_path` pair, so — exactly like the
//! upstream kubelet `containerLogManager` — rotation must be EXTERNAL:
//! copy the file aside, then truncate the original in place (copytruncate)
//! so the writer's open fd stays valid. Q31 already accepted this as the
//! sanctioned deviation from k3s (which leans on containerd's own limits).
//!
//! Layout (the write side lives in [`crate::sync::sandbox_cfg`]):
//! `<data_dir>/pods/<pod-uid>/<YYYY-MM-DD>/<container>.<attempt>.log` —
//! the date is pinned when the sandbox config is planned and stays fixed
//! for the container's life (containerd opens the fd once). Restarts after
//! a midnight rollover land in the new date directory; the read side
//! ([`crate::server::resolve_log_path`]) picks the newest date directory
//! and the highest attempt inside it.
//!
//! Rotation policy: any live `*.log` file >= [`LOG_MAX_SIZE`] (100 MiB) is
//! rotated once per sweep — copies shift `.1`->`.2`... with at most
//! [`LOG_MAX_FILES`] generations retained — then the original is
//! truncated. The sweep walks ONLY date-shaped directories under
//! `<log_root>/<pod-uid>/` so sibling directories (`volumes/`, foreign
//! files) are never touched. Pure helpers keep the tricky parts
//! unit-testable; the sweep itself is sync `std::fs` (the runner wraps
//! it in `spawn_blocking`).

use std::collections::BTreeSet;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Per-file rotation threshold: 100 MiB (Q39; upstream's default is 10 MiB
/// — this deployment profile wants day-sized chunks instead).
pub const LOG_MAX_SIZE: u64 = 100 * 1024 * 1024;

/// Retained generations per log file after rotation (`.1` ..= `.5`).
pub const LOG_MAX_FILES: usize = 5;

/// How often the runner's rotation task sweeps the log tree.
pub const SCAN_PERIOD: Duration = Duration::from_secs(60);

/// One sweep's knobs; [`Default`] mirrors the k3s-like constants above.
/// `Copy` so the runner task can hand it to `spawn_blocking`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogRotatePolicy {
    pub max_size: u64,
    pub max_files: usize,
    pub scan_period: Duration,
}

impl Default for LogRotatePolicy {
    fn default() -> Self {
        Self {
            max_size: LOG_MAX_SIZE,
            max_files: LOG_MAX_FILES,
            scan_period: SCAN_PERIOD,
        }
    }
}

/// UTC `"YYYY-MM-DD"` for a `SystemTime` (pure; Hinnant civil date via
/// `common::time`, no chrono). Pre-epoch clocks clamp to 1970-01-01 — a
/// broken clock must never yield a non-date directory name.
pub fn today(now: SystemTime) -> String {
    let secs = now
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    common::time::utc_date(secs)
}

/// The sandbox `log_directory` for one pod on one date:
/// `<log_root>/<uid>/<YYYY-MM-DD>`.
pub fn dated_dir(log_root: &Path, uid: &str, date: &str) -> PathBuf {
    log_root.join(uid).join(date)
}

/// Exactly `YYYY-MM-DD` with a plausible month/day (01-12 / 01-31). This
/// is the guard that keeps `volumes/` and other sibling directories out
/// of both the read side's newest-date pick and this module's sweep.
pub fn is_date_dir(name: &str) -> bool {
    let b = name.as_bytes();
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return false;
    }
    let digit = |r: std::ops::Range<usize>| {
        name.get(r)
            .is_some_and(|t| t.bytes().all(|c| c.is_ascii_digit()))
    };
    if !(digit(0..4) && digit(5..7) && digit(8..10)) {
        return false;
    }
    let m: u32 = name[5..7].parse().unwrap_or(0);
    let d: u32 = name[8..10].parse().unwrap_or(0);
    (1..=12).contains(&m) && (1..=31).contains(&d)
}

/// The rotated-generation path for one log file: `<file>.<gen>`
/// (`web.0.log` -> `web.0.log.1`).
fn gen_path(path: &Path, gen: u32) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(format!(".{gen}"));
    PathBuf::from(s)
}

/// One copytruncate pass over a single file: delete the oldest retained
/// generation, shift the rest up one slot, copy the live file into `.1`,
/// then truncate the live file in place (writer fds stay valid — Q39).
pub fn rotate_one(path: &Path, keep: usize) -> io::Result<()> {
    let keep = keep.max(1);
    let existing: BTreeSet<u32> = (1..=keep as u32)
        .filter(|g| gen_path(path, *g).is_file())
        .collect();
    let plan = rotation_plan(path, keep, &existing);
    for victim in &plan.removes {
        std::fs::remove_file(victim)?;
    }
    for (from, to) in &plan.renames {
        std::fs::rename(from, to)?;
    }
    std::fs::copy(path, gen_path(path, 1))?;
    // Truncate, do not delete: the container runtime holds this fd open.
    std::fs::File::create(path)?;
    Ok(())
}

/// Pure: which generation copies to delete and which to rename for one
/// rotation of `path`, given the generations currently on disk.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Rotation {
    pub removes: Vec<PathBuf>,
    /// (from, to) pairs, ordered oldest-first.
    pub renames: Vec<(PathBuf, PathBuf)>,
}

pub fn rotation_plan(path: &Path, keep: usize, existing: &BTreeSet<u32>) -> Rotation {
    let mut plan = Rotation::default();
    let keep = keep.max(1) as u32;
    if existing.contains(&keep) {
        plan.removes.push(gen_path(path, keep));
    }
    // Highest generation shifts first so no slot is overwritten before it
    // has been vacated (and deletes-before-renames keeps this valid even
    // on platforms where rename does not clobber the destination).
    for g in (1..keep).rev() {
        if existing.contains(&g) {
            plan.renames
                .push((gen_path(path, g), gen_path(path, g + 1)));
        }
    }
    plan
}

/// One sweep over `<log_root>/<pod-uid>/<date>/`: every live `*.log`
/// (rotated copies end `.log.N` and never match) at or above the policy
/// size is copytruncated. Returns how many files were rotated. Errors on
/// individual files are logged (target `init-pro`) and never abort the
/// sweep — the next tick retries.
pub fn sweep(root: &Path, policy: &LogRotatePolicy) -> usize {
    let mut rotated = 0;
    let Ok(pods) = std::fs::read_dir(root) else {
        return 0;
    };
    for pod in pods.flatten() {
        if !is_plain_dir(&pod) {
            continue;
        }
        let Ok(dates) = std::fs::read_dir(pod.path()) else {
            continue;
        };
        for date in dates.flatten() {
            let name = date.file_name();
            let name = name.to_string_lossy();
            if !is_plain_dir(&date) || !is_date_dir(&name) {
                continue;
            }
            let Ok(files) = std::fs::read_dir(date.path()) else {
                continue;
            };
            for file in files.flatten() {
                let fname = file.file_name();
                let fname = fname.to_string_lossy();
                if !fname.ends_with(".log") {
                    continue;
                }
                let oversized = file
                    .metadata()
                    .map(|m| m.len() >= policy.max_size)
                    .unwrap_or(false);
                if !oversized {
                    continue;
                }
                match rotate_one(&file.path(), policy.max_files) {
                    Ok(()) => rotated += 1,
                    Err(e) => tracing::warn!(
                        target: "init-pro",
                        path = %file.path().display(),
                        error = %e,
                        "kubelet: log rotation failed (retried next sweep)"
                    ),
                }
            }
        }
    }
    rotated
}

/// A real directory (not a symlink — `file_type` does not follow links).
fn is_plain_dir(entry: &std::fs::DirEntry) -> bool {
    entry.file_type().map(|t| t.is_dir()).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "kubelet-logrotate-{}-{}-{tag}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn today_is_fixed_width_utc_date() {
        assert_eq!(today(UNIX_EPOCH), "1970-01-01");
        assert_eq!(
            today(SystemTime::UNIX_EPOCH + Duration::from_secs(951_782_400)),
            "2000-02-29"
        );
        // Mid-instant inside one day never leaks time components.
        assert_eq!(
            today(UNIX_EPOCH + Duration::from_secs(86_399)),
            "1970-01-01"
        );
    }

    #[test]
    fn dated_dir_composes_the_t4_6_layout() {
        assert_eq!(
            dated_dir(Path::new("/d/pods"), "u1", "2026-11-06"),
            PathBuf::from("/d/pods/u1/2026-11-06")
        );
    }

    #[test]
    fn is_date_dir_accepts_only_date_shapes() {
        for good in ["1970-01-01", "2026-11-06", "9999-12-31"] {
            assert!(is_date_dir(good), "{good} must pass");
        }
        for bad in [
            "volumes",
            "",
            "2026-1-06",
            "2026-11-6",
            "20261106",
            "2026-13-01",
            "2026-11-32",
            "2026-00-10",
            "2026-11-00",
            "202a-11-06",
            "2026-11-06 ",
        ] {
            assert!(!is_date_dir(bad), "{bad:?} must be rejected");
        }
    }

    #[test]
    fn rotation_plan_shifts_and_caps_the_retained_generations() {
        let p = Path::new("/d/pods/u1/2026-11-06/web.0.log");
        // Empty disk: nothing to move, the copy into .1 happens in rotate_one.
        assert_eq!(rotation_plan(p, 5, &BTreeSet::new()), Rotation::default());
        // Generations 1,2,4 present, keep=5: nothing evicted, 4->5, 2->3, 1->2.
        let existing: BTreeSet<u32> = [1, 2, 4].into_iter().collect();
        let plan = rotation_plan(p, 5, &existing);
        assert!(plan.removes.is_empty());
        assert_eq!(
            plan.renames,
            vec![
                (gen_path(p, 4), gen_path(p, 5)),
                (gen_path(p, 2), gen_path(p, 3)),
                (gen_path(p, 1), gen_path(p, 2)),
            ]
        );
        // Eviction: .5 present gets deleted (not renamed into .6).
        let full: BTreeSet<u32> = [1, 2, 3, 4, 5].into_iter().collect();
        let plan = rotation_plan(p, 5, &full);
        assert_eq!(plan.removes, vec![gen_path(p, 5)]);
        assert_eq!(
            plan.renames.first().unwrap(),
            &(gen_path(p, 4), gen_path(p, 5))
        );
        assert_eq!(plan.renames.len(), 4);
        // Degenerate keep=0 is clamped to 1.
        let plan = rotation_plan(p, 0, &[1].into_iter().collect());
        assert_eq!(plan.removes, vec![gen_path(p, 1)]);
        assert!(plan.renames.is_empty());
    }

    #[test]
    fn rotate_one_copies_truncates_and_retains() {
        let dir = tmp("one");
        let live = dir.join("web.0.log");
        std::fs::write(&live, b"AAAA").unwrap();
        std::fs::write(dir.join("web.0.log.1"), b"gen1").unwrap();
        std::fs::write(dir.join("web.0.log.5"), b"gen5").unwrap();
        rotate_one(&live, 5).unwrap();
        assert_eq!(
            std::fs::read(&live).unwrap(),
            b"",
            "copytruncate empties the live file"
        );
        assert_eq!(std::fs::read(dir.join("web.0.log.1")).unwrap(), b"AAAA");
        assert_eq!(std::fs::read(dir.join("web.0.log.2")).unwrap(), b"gen1");
        assert!(
            !dir.join("web.0.log.5").exists(),
            "oldest generation evicted"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sweep_rotates_only_oversized_logs_in_date_dirs() {
        let dir = tmp("sweep");
        let date = dir.join("u1").join("2026-11-06");
        std::fs::create_dir_all(&date).unwrap();
        let big = date.join("web.0.log");
        std::fs::write(&big, vec![b'x'; 40]).unwrap();
        std::fs::write(date.join("small.0.log"), b"tiny").unwrap();
        // Legacy flat layout + a volumes sibling: both must stay untouched.
        let vol = dir.join("u1").join("volumes").join("emptyDir").join("v");
        std::fs::create_dir_all(&vol).unwrap();
        std::fs::write(vol.join("data"), vec![b'y'; 40]).unwrap();
        let flat = dir.join("u1").join("legacy.0.log");
        std::fs::write(&flat, vec![b'z'; 40]).unwrap();

        let policy = LogRotatePolicy {
            max_size: 40,
            ..LogRotatePolicy::default()
        };
        assert_eq!(sweep(&dir, &policy), 1);
        assert!(big.exists() && std::fs::read(&big).unwrap().is_empty());
        assert_eq!(std::fs::read(date.join("web.0.log.1")).unwrap().len(), 40);
        assert_eq!(
            std::fs::read(&flat).unwrap(),
            vec![b'z'; 40],
            "flat legacy file not rotated"
        );
        assert_eq!(
            std::fs::read(vol.join("data")).unwrap(),
            vec![b'y'; 40],
            "volume data untouched"
        );
        // Idempotent: everything is now under the threshold.
        assert_eq!(sweep(&dir, &policy), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sweep_retains_at_most_max_files_generations() {
        let dir = tmp("cap");
        let date = dir.join("u9").join("2026-01-02");
        std::fs::create_dir_all(&date).unwrap();
        let live = date.join("c.0.log");
        std::fs::write(&live, vec![b'a'; 10]).unwrap();
        for g in 1..=5 {
            std::fs::write(gen_path(&live, g), vec![b'g'; 2]).unwrap();
        }
        let policy = LogRotatePolicy {
            max_size: 10,
            max_files: 5,
            ..LogRotatePolicy::default()
        };
        assert_eq!(sweep(&dir, &policy), 1);
        for g in 1..=5 {
            assert!(gen_path(&live, g).is_file(), "generation .{g} retained");
        }
        assert!(
            !gen_path(&live, 6).exists(),
            "cap enforced at 5 generations"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sweep_on_missing_root_is_a_noop() {
        assert_eq!(
            sweep(
                Path::new("/nonexistent-kubelet-logrotate"),
                &LogRotatePolicy::default()
            ),
            0
        );
    }
}
