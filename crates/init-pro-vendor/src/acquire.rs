//! Acquire orchestrator (T0.2 Q6): download pinned upstream artifacts, verify
//! each against its recorded SHA-256, and stage into `vendor/bin/`.
//!
//! Modes (precedence: OFFLINE > VENDOR > AUTO):
//! - **Auto** (default): use the cache if present; otherwise *skip* + warn
//!   (no network). Keeps `cargo build`/`cargo test` network-free and fast.
//! - **Vendor** (`INIT_PRO_VENDOR=1`): download missing artifacts.
//! - **Offline** (`INIT_PRO_OFFLINE=1`): never touch network; a missing
//!   artifact is a hard error (the air-gap contract: vendor/bin must be
//!   pre-populated).
//!
//! The pure `plan()` is unit-tested without I/O; the download/extract shims
//! (`curl`, `tar`) mirror k3s's shell-based acquire and are verified e2e.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::digest;
use crate::manifest::Artifact;

/// Acquire mode resolved from the environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Auto,
    Vendor,
    Offline,
}

/// Resolve the acquire mode from env (precedence OFFLINE > VENDOR > AUTO).
pub fn mode_from_env() -> Mode {
    if env_truthy("INIT_PRO_OFFLINE") {
        Mode::Offline
    } else if env_truthy("INIT_PRO_VENDOR") {
        Mode::Vendor
    } else {
        Mode::Auto
    }
}

fn env_truthy(key: &str) -> bool {
    matches!(
        std::env::var(key),
        Ok(v) if v == "1" || v.eq_ignore_ascii_case("true")
    )
}

/// What to do with one artifact, given cache state + mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Cached,
    Download,
    Skip,
    MissingOffline,
}

/// Pure decision function (no I/O) — the unit-testable core.
pub fn plan(hits: &[bool], mode: Mode) -> Vec<Action> {
    hits.iter()
        .map(|&h| {
            if h {
                Action::Cached
            } else {
                match mode {
                    Mode::Auto => Action::Skip,
                    Mode::Vendor => Action::Download,
                    Mode::Offline => Action::MissingOffline,
                }
            }
        })
        .collect()
}

/// Result of an acquire run.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Report {
    pub total: usize,
    pub cached: usize,
    pub downloaded: usize,
    pub skipped: usize,
}

impl Report {
    pub fn summary(&self) -> String {
        format!(
            "{} artifacts: {} cached, {} downloaded, {} skipped",
            self.total, self.cached, self.downloaded, self.skipped
        )
    }
}

/// Run the full acquire over all artifacts under `vendor_root`.
///
/// `vendor_root` is the repo `vendor/` dir; the cache is `vendor/cache/` and
/// staged files land under `vendor/bin/`.
pub fn run(
    vendor_root: &Path,
    artifacts: &[Artifact],
    mode: Mode,
) -> Result<Report, AcquireError> {
    let cache_dir = vendor_root.join("cache");
    std::fs::create_dir_all(&cache_dir).map_err(|e| AcquireError::io("cache dir", e))?;
    std::fs::create_dir_all(vendor_root.join("bin")).map_err(|e| AcquireError::io("bin dir", e))?;

    let hits: Vec<bool> = artifacts
        .iter()
        .map(|a| cache_hit(&cache_dir, a))
        .collect();
    let actions = plan(&hits, mode);

    let mut rep = Report {
        total: artifacts.len(),
        ..Default::default()
    };
    for (art, act) in artifacts.iter().zip(actions) {
        match act {
            Action::Cached => {
                stage(art, &cache_dir, vendor_root)?;
                rep.cached += 1;
            }
            Action::Download => {
                download_to_cache(art, &cache_dir)?;
                stage(art, &cache_dir, vendor_root)?;
                rep.downloaded += 1;
            }
            Action::Skip => rep.skipped += 1,
            Action::MissingOffline => {
                return Err(AcquireError::OfflineMissing {
                    name: art.name.clone(),
                    url: art.url.clone(),
                });
            }
        }
    }
    Ok(rep)
}

/// True iff the cached copy exists and matches the recorded SHA-256.
fn cache_hit(cache_dir: &Path, art: &Artifact) -> bool {
    let p = cache_dir.join(art.cache_name());
    p.is_file() && digest::verify_file(&p, &art.sha256).unwrap_or(false)
}

/// Download `art` into the cache, verifying SHA-256 before committing.
fn download_to_cache(art: &Artifact, cache_dir: &Path) -> Result<(), AcquireError> {
    let dest = cache_dir.join(art.cache_name());
    let tmp = cache_dir.join(format!(".{}.part", art.cache_name()));
    // Clean any stale partial.
    let _ = std::fs::remove_file(&tmp);

    let st = Command::new("curl")
        .args(["-fSL", "--retry", "5", "--retry-delay", "3", "--connect-timeout", "20", "--max-time", "600", "-o"])
        .arg(&tmp)
        .arg(&art.url)
        .status()
        .map_err(|e| AcquireError::DownloadFailed {
            name: art.name.clone(),
            source: e,
        })?;
    if !st.success() {
        let _ = std::fs::remove_file(&tmp);
        return Err(AcquireError::DownloadFailed {
            name: art.name.clone(),
            source: std::io::Error::other(format!("curl exited {st}")),
        });
    }
    let actual = digest::sha256_file(&tmp)
        .map_err(|e| AcquireError::io("hash downloaded file", e))?;
    if !actual.eq_ignore_ascii_case(&art.sha256) {
        let _ = std::fs::remove_file(&tmp);
        return Err(AcquireError::ShaMismatch {
            name: art.name.clone(),
            expected: art.sha256.clone(),
            actual,
        });
    }
    std::fs::rename(&tmp, &dest).map_err(|e| AcquireError::io("commit cache file", e))?;
    Ok(())
}

/// Stage a cached artifact into `vendor/bin/` (extract tar / copy bin).
fn stage(art: &Artifact, cache_dir: &Path, vendor_root: &Path) -> Result<(), AcquireError> {
    let src = cache_dir.join(art.cache_name());
    let dest_dir = dest_dir(vendor_root, art);
    std::fs::create_dir_all(&dest_dir).map_err(|e| AcquireError::io("stage dest dir", e))?;
    match art.kind {
        crate::manifest::Kind::Tar => {
            let mut cmd = Command::new("tar");
            cmd.arg("xpf").arg(&src).arg("-C").arg(&dest_dir);
            if art.strip > 0 {
                cmd.arg(format!("--strip-components={}", art.strip));
            }
            let st = cmd
                .status()
                .map_err(|e| AcquireError::ExtractFailed {
                    name: art.name.clone(),
                    source: e,
                })?;
            if !st.success() {
                return Err(AcquireError::ExtractFailed {
                    name: art.name.clone(),
                    source: std::io::Error::other(format!("tar exited {st}")),
                });
            }
        }
        crate::manifest::Kind::Bin => {
            let target = dest_dir.join(art.bin_name());
            std::fs::copy(&src, &target)
                .map_err(|e| AcquireError::ExtractFailed {
                    name: art.name.clone(),
                    source: e,
                })?;
            chmod_exec(&target);
        }
    }
    Ok(())
}

/// Resolve the staging destination dir for an artifact under `vendor/bin/`.
fn dest_dir(vendor_root: &Path, art: &Artifact) -> PathBuf {
    let bin = vendor_root.join("bin");
    if art.into == "." || art.into.is_empty() {
        bin
    } else {
        bin.join(&art.into)
    }
}

#[cfg(unix)]
fn chmod_exec(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755));
}
#[cfg(not(unix))]
fn chmod_exec(_path: &Path) {}

/// Acquire failure.
#[derive(Debug)]
pub enum AcquireError {
    OfflineMissing { name: String, url: String },
    DownloadFailed { name: String, source: std::io::Error },
    ShaMismatch { name: String, expected: String, actual: String },
    ExtractFailed { name: String, source: std::io::Error },
    Io { ctx: String, source: std::io::Error },
}

impl AcquireError {
    fn io(ctx: &str, source: std::io::Error) -> Self {
        AcquireError::Io {
            ctx: ctx.to_string(),
            source,
        }
    }
    /// Human message + the suggested fix.
    pub fn hint(&self) -> &'static str {
        match self {
            AcquireError::OfflineMissing { .. } => {
                "pre-populate vendor/bin/ (e.g. run `INIT_PRO_VENDOR=1 cargo build`) \
                 or unset INIT_PRO_OFFLINE"
            }
            _ => "",
        }
    }
}

impl std::fmt::Display for AcquireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AcquireError::OfflineMissing { name, url } => write!(
                f,
                "offline mode: artifact `{name}` not in cache (would fetch {url})"
            ),
            AcquireError::DownloadFailed { name, source } => {
                write!(f, "download of `{name}` failed: {source}")
            }
            AcquireError::ShaMismatch {
                name,
                expected,
                actual,
            } => write!(
                f,
                "SHA-256 mismatch for `{name}`: expected {expected}, got {actual}"
            ),
            AcquireError::ExtractFailed { name, source } => {
                write!(f, "staging `{name}` failed: {source}")
            }
            AcquireError::Io { ctx, source } => write!(f, "{ctx}: {source}"),
        }
    }
}
impl std::error::Error for AcquireError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_auto_skips_misses() {
        assert_eq!(plan(&[true, false], Mode::Auto), vec![Action::Cached, Action::Skip]);
    }

    #[test]
    fn plan_vendor_downloads_misses() {
        assert_eq!(
            plan(&[false, true], Mode::Vendor),
            vec![Action::Download, Action::Cached]
        );
    }

    #[test]
    fn plan_offline_errors_misses() {
        assert_eq!(
            plan(&[false], Mode::Offline),
            vec![Action::MissingOffline]
        );
    }

    #[test]
    fn plan_all_cached() {
        assert_eq!(
            plan(&[true, true, true], Mode::Offline),
            vec![Action::Cached, Action::Cached, Action::Cached]
        );
    }

    #[test]
    fn mode_precedence_offline_over_vendor() {
        std::env::set_var("INIT_PRO_OFFLINE", "1");
        std::env::set_var("INIT_PRO_VENDOR", "1");
        assert_eq!(mode_from_env(), Mode::Offline);
        std::env::remove_var("INIT_PRO_OFFLINE");
        std::env::remove_var("INIT_PRO_VENDOR");
    }

    #[test]
    fn mode_default_auto() {
        std::env::remove_var("INIT_PRO_OFFLINE");
        std::env::remove_var("INIT_PRO_VENDOR");
        assert_eq!(mode_from_env(), Mode::Auto);
    }

    #[test]
    fn report_summary_format() {
        let r = Report {
            total: 3,
            cached: 1,
            downloaded: 1,
            skipped: 1,
        };
        assert_eq!(
            r.summary(),
            "3 artifacts: 1 cached, 1 downloaded, 1 skipped"
        );
    }

    #[test]
    fn offline_missing_hint() {
        let e = AcquireError::OfflineMissing {
            name: "x".into(),
            url: "u".into(),
        };
        assert!(e.hint().contains("INIT_PRO_VENDOR"));
        assert!(e.to_string().contains("offline mode"));
    }
}
