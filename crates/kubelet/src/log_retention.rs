//! Age-based retention of per-date pod-log directories (TODO **T4.6**, Q39;
//! C4).
//!
//! [`crate::log_rotate`] rotates OVERSIZED files but never retires a whole
//! date directory, so `<log_root>/<uid>/<YYYY-MM-DD>/` accumulated forever on
//! a long-lived node (T4.6 only ever deleted the rotated generations it
//! replaced). This module adds the missing half: a date directory older than
//! [`STALE_LOG_DAYS`] is deleted -- with two guards, because the date is
//! pinned when the container starts (Q39):
//!
//! 1. the CURRENT date is never considered stale ([`is_stale_date`]), so
//!    today's live logs are untouchable, and
//! 2. a directory that still has a recently-written log file inside it is
//!    kept even when its date is old ([`is_stale_dir`]) -- a container that
//!    has been running for a week still appends to the file it opened on day
//!    one, and deleting that directory would silently unlink its log.
//!
//! Non-date siblings (`volumes/`, foreign files) are skipped by the same
//! shape check the rotation sweep uses. Errors are values: a failed removal
//! is logged and retried on the next sweep.

use std::path::Path;
use std::time::{Duration, SystemTime};

use crate::log_rotate::is_date_dir;

/// Retention window for a per-date pod-log directory (C4): a date dir whose
/// day is more than this many days before today is a candidate for removal.
pub const STALE_LOG_DAYS: u32 = 7;

/// Hinnant `days_from_civil` (the inverse of `common::time`'s
/// `civil_from_days`): the day number of a `YYYY-MM-DD` date, so two dates
/// compare as plain integers. No chrono, per repo convention.
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let mp = (m as i64 + 9) % 12; // [0, 11]
    let doy = (153 * mp + 2) / 5 + d as i64 - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
}

/// Day number of a date-shaped directory name, or `None` when it is not a
/// plausible `YYYY-MM-DD` (the same shape check as the rotation sweep).
pub fn day_of(date_dir_name: &str) -> Option<i64> {
    if !is_date_dir(date_dir_name) {
        return None;
    }
    let y: i64 = date_dir_name[0..4].parse().ok()?;
    let m: u32 = date_dir_name[5..7].parse().ok()?;
    let d: u32 = date_dir_name[8..10].parse().ok()?;
    Some(days_from_civil(y, m, d))
}

/// Pure decision (C4): is `date` (a `YYYY-MM-DD` directory name) older than
/// `retain_days` before `today`? `today` is injected so the rule is
/// testable; the current date is never stale, whatever the window.
pub fn is_stale_date(date: &str, today: &str, retain_days: u32) -> bool {
    match (day_of(date), day_of(today)) {
        (Some(d), Some(now)) => now - d > i64::from(retain_days),
        _ => false, // not a date dir -> never a retention candidate
    }
}

/// Newest mtime among the files directly inside `dir` (the live writer's
/// fingerprint). An empty or unreadable dir yields `None`, which the caller
/// treats as "nothing recent".
fn newest_mtime(dir: &Path) -> Option<SystemTime> {
    let mut newest: Option<SystemTime> = None;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let Ok(mtime) = meta.modified() else {
            continue;
        };
        newest = Some(match newest {
            Some(cur) if cur > mtime => cur,
            _ => mtime,
        });
    }
    newest
}

/// Pure-ish decision (C4): a candidate dir is only stale when its newest
/// file is older than `cutoff` too (guard 2 in the module doc).
pub fn is_stale_dir(dir: &Path, cutoff: SystemTime) -> bool {
    match newest_mtime(dir) {
        Some(mtime) => mtime < cutoff,
        None => false, // empty/unreadable: leave it to the operator
    }
}

/// Remove every per-date log dir under `<log_root>/<uid>/` that is both
/// older than the window and idle since `cutoff`. Returns how many
/// directories were removed. The sweep is sync `std::fs` -- the runner wraps
/// it in `spawn_blocking` next to the rotation pass.
pub fn sweep_stale(log_root: &Path, today: &str, retain_days: u32, cutoff: SystemTime) -> usize {
    let mut removed = 0usize;
    let Ok(pods) = std::fs::read_dir(log_root) else {
        return 0;
    };
    for pod in pods.flatten() {
        // A real directory, never a symlink (`file_type` does not follow).
        if !pod.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let Ok(dates) = std::fs::read_dir(pod.path()) else {
            continue;
        };
        for date in dates.flatten() {
            if !date.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let Some(name) = date.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if !is_stale_date(&name, today, retain_days) {
                continue;
            }
            if !is_stale_dir(&date.path(), cutoff) {
                continue;
            }
            match std::fs::remove_dir_all(date.path()) {
                Ok(()) => removed += 1,
                Err(e) => tracing::warn!(
                    target: "init-pro",
                    dir = %date.path().display(),
                    error = %e,
                    "kubelet: could not remove a stale pod-log date dir (retried next sweep)"
                ),
            }
        }
    }
    removed
}

/// One retention pass over `log_root` with the default window and a cutoff
/// derived from `now` -- the face the runner's rotation task calls.
pub fn sweep(log_root: &Path, now: SystemTime, today: &str) -> usize {
    let cutoff = now
        .checked_sub(Duration::from_secs(u64::from(STALE_LOG_DAYS) * 86_400))
        .unwrap_or(SystemTime::UNIX_EPOCH);
    sweep_stale(log_root, today, STALE_LOG_DAYS, cutoff)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{File, FileTimes};
    use std::path::PathBuf;

    fn tmp(tag: &str) -> PathBuf {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "kubelet-logret-{}-{}-{tag}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// `path` with every file inside stamped `at` (the "old" or "fresh"
    /// writer state).
    fn stamp(dir: &Path, at: SystemTime) {
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            if entry.metadata().unwrap().is_file() {
                File::options()
                    .write(true)
                    .open(entry.path())
                    .unwrap()
                    .set_times(FileTimes::new().set_modified(at))
                    .unwrap();
            }
        }
    }

    fn log_dir(root: &Path, uid: &str, date: &str) -> PathBuf {
        let dir = root.join(uid).join(date);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("web.0.log"), b"log line\n").unwrap();
        dir
    }

    #[test]
    fn days_from_civil_matches_the_known_epoch() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 2, 29), 11_016);
        assert_eq!(days_from_civil(2026, 11, 26), 20_783);
        assert_eq!(day_of("1970-01-01"), Some(0));
        assert_eq!(day_of("volumes"), None, "non-date dirs are invisible");
        assert_eq!(day_of("2026-13-40"), None, "implausible dates rejected");
    }

    #[test]
    fn is_stale_date_keeps_today_and_the_window() {
        let today = "2026-11-26";
        assert!(
            !is_stale_date("2026-11-26", today, 7),
            "today is never stale"
        );
        assert!(
            !is_stale_date("2026-11-19", today, 7),
            "7 days old is inside the window"
        );
        assert!(is_stale_date("2026-11-18", today, 7), "8 days old is stale");
        assert!(is_stale_date("2020-01-01", today, 7));
        assert!(!is_stale_date("volumes", today, 7));
        // A future date (clock skew) is never stale either.
        assert!(!is_stale_date("2027-01-01", today, 7));
    }

    #[test]
    fn sweep_removes_old_idle_dirs_and_keeps_live_ones() {
        let root = tmp("sweep");
        let old = log_dir(&root, "u1", "2020-01-01");
        let today = log_dir(&root, "u1", "2026-11-26");
        // A week-old dir that is STILL being written: a long-running
        // container appends to the file it opened on day one.
        let live = log_dir(&root, "u2", "2020-01-02");
        // `volumes/` sits next to the date dirs and must never be touched.
        let volumes = root.join("u1").join("volumes");
        std::fs::create_dir_all(&volumes).unwrap();
        std::fs::write(volumes.join("emptyDir"), b"x").unwrap();

        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        stamp(
            &old,
            SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000),
        );
        stamp(&today, now);
        stamp(&live, now);
        stamp(
            &volumes,
            SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000),
        );

        let removed = sweep(&root, now, "2026-11-26");
        assert_eq!(removed, 1, "only the old idle dir goes");
        assert!(!old.exists(), "an old idle date dir is retired (C4)");
        assert!(today.exists(), "the current date dir survives");
        assert!(
            live.exists(),
            "a dir with a fresh writer survives (Q39 pinned fd)"
        );
        assert!(
            volumes.join("emptyDir").is_file(),
            "non-date dirs untouched"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn sweep_tolerates_a_missing_root_and_is_idempotent() {
        assert_eq!(
            sweep(
                Path::new("/nonexistent/init-pro-logret"),
                SystemTime::now(),
                "2026-11-26"
            ),
            0
        );
        let root = tmp("idempotent");
        let now = SystemTime::now();
        assert_eq!(sweep(&root, now, "2026-11-26"), 0);
        let _ = std::fs::remove_dir_all(&root);
    }
}
