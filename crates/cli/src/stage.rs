//! Runtime staging (T0.2 B5, k3s `extract()` parity).
//!
//! Decompresses zstd-embedded blobs into a versioned data directory, verifies
//! every file against its SHA-256 pin, then atomically commits via directory
//! rename + `data/current` symlink rotation. Idempotent: if `current` already
//! points at the same hash, the fast path skips all I/O.

#![forbid(unsafe_code)]

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use common::embed::EmbeddedAsset;
use infra::Config;
use sha2::{Digest, Sha256};
use std::fs::File;

/// Result of a staging operation.
#[derive(Debug, Clone)]
pub struct StageResult {
    /// SHA-256 hex of the `.sha256sums` content (bundle version identifier).
    pub hash: String,
    /// `true` if files were written this run; `false` = fast-path skip.
    pub staged: bool,
    /// `data/current` absolute path.
    pub current: PathBuf,
    /// PATH entries for child processes (CNI aux dir, bin dir).
    pub path_entries: Vec<PathBuf>,
}

/// Stage embedded assets to `<data-dir>/data/` (k3s extract() parity).
///
/// Flow: flock → write-tmp → dataverify → atomic-rename → symlink rotation.
/// Idempotent: skips if `data/current` already resolves to this hash.
pub fn stage(
    cfg: &Config,
    assets: &[EmbeddedAsset],
    sha256_sums: &str,
) -> Result<StageResult, StageError> {
    if assets.is_empty() {
        return Err(StageError::no_assets());
    }

    let data_dir = cfg.data_dir.join("data");
    fs::create_dir_all(&data_dir).map_err(|e| StageError::io("create data dir", e))?;

    // 1. Compute the bundle hash from the sha256sums content.
    let hash = compute_data_hash(sha256_sums);

    // 2. Fast path: current already points at this hash?
    let current_link = data_dir.join("current");
    if fast_path_hit(&current_link, &hash) {
        return Ok(StageResult {
            hash,
            staged: false,
            path_entries: path_entries(&current_link),
            current: current_link,
        });
    }

    // 3. Acquire flock (prevents concurrent staging).
    let lock_path = data_dir.join(".lock");
    let lock = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .read(true)
        .open(&lock_path)
        .map_err(|e| StageError::io("open lock file", e))?;
    lock.lock().map_err(|e| StageError::io("flock", e))?;

    // 4. Write to <hash>-tmp/ (clean any stale tmp first).
    let tmp_dir = data_dir.join(format!("{hash}-tmp"));
    let _ = fs::remove_dir_all(&tmp_dir);
    fs::create_dir_all(&tmp_dir).map_err(|e| StageError::io("create tmp dir", e))?;

    for asset in assets {
        write_asset(&tmp_dir, asset)?;
    }

    // 5. Dataverify: recompute SHA-256 of every staged file.
    verify_assets(&tmp_dir, assets)?;

    // 6. Atomic rename <hash>-tmp/ → <hash>/.
    let final_dir = data_dir.join(&hash);
    let _ = fs::remove_dir_all(&final_dir);
    fs::rename(&tmp_dir, &final_dir).map_err(|e| StageError::io("atomic rename", e))?;

    // 7. Symlink rotation: old current → previous, new current → <hash>.
    rotate_symlinks(&data_dir, &current_link, &hash)?;

    // C8: the rotation just orphaned the bundle the OLD `previous` pointed
    // at. Prune it (best-effort, never the two live ones) before the lock
    // goes, so upgrades stop leaking one hundreds-of-MB bundle each.
    prune_stale_bundles(&data_dir, &hash);

    // Release flock (drop).
    drop(lock);

    Ok(StageResult {
        hash,
        staged: true,
        path_entries: path_entries(&current_link),
        current: current_link,
    })
}

/// SHA-256 hex of the sha256sums content (the bundle version key).
fn compute_data_hash(sha256_sums: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(sha256_sums.as_bytes());
    hex_encode(&hasher.finalize())
}

/// Check if `current` symlink already resolves to `<hash>`.
fn fast_path_hit(current: &Path, hash: &str) -> bool {
    match fs::read_link(current) {
        Ok(target) => target.file_name() == Some(std::ffi::OsStr::new(hash)),
        Err(_) => false,
    }
}

/// Decompress + write one embedded asset to `base/<path>`.
fn write_asset(base: &Path, asset: &EmbeddedAsset) -> Result<(), StageError> {
    let dest = base.join(asset.path);
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).map_err(|e| StageError::io("create parent dir", e))?;
    }

    let decompressed =
        zstd::decode_all(asset.zstd).map_err(|e| StageError::io("zstd decompress", e))?;

    if decompressed.len() as u64 != asset.size {
        return Err(StageError::size_mismatch(
            asset.path,
            asset.size,
            decompressed.len() as u64,
        ));
    }

    let mut f = fs::File::create(&dest).map_err(|e| StageError::io("create staged file", e))?;
    f.write_all(&decompressed)
        .map_err(|e| StageError::io("write staged file", e))?;
    drop(f);

    set_mode(&dest, asset.path);
    Ok(())
}

/// Verify all staged files match their expected SHA-256.
fn verify_assets(base: &Path, assets: &[EmbeddedAsset]) -> Result<(), StageError> {
    for asset in assets {
        let path = base.join(asset.path);
        let data = fs::read(&path).map_err(|e| StageError::io("read for verify", e))?;
        let mut hasher = Sha256::new();
        hasher.update(&data);
        let actual = hex_encode(&hasher.finalize());
        if actual != asset.sha256 {
            return Err(StageError::hash_mismatch(asset.path, asset.sha256, &actual));
        }
    }
    Ok(())
}

/// Rotate symlinks: move old `current` → `previous`, set `current` → `<hash>`.
fn rotate_symlinks(data_dir: &Path, current: &Path, hash: &str) -> Result<(), StageError> {
    let previous = data_dir.join("previous");
    let _ = fs::remove_file(&previous);

    // If old current exists, move it to previous.
    if current.exists() || fs::symlink_metadata(current).is_ok() {
        let _ = fs::rename(current, &previous);
    }

    #[cfg(unix)]
    std::os::unix::fs::symlink(hash, current)
        .map_err(|e| StageError::io("create current symlink", e))?;
    Ok(())
}

/// Set executable bit for binaries (path-based heuristic).
fn set_mode(path: &Path, logical: &str) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = if is_executable(logical) { 0o755 } else { 0o644 };
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode));
    }
}

/// C8: `true` for a staged-bundle directory name: exactly 64 lowercase
/// hex chars, the SHA-256 `compute_data_hash` mints from the pin file.
/// `current`, `previous`, `.lock` and `<hash>-tmp` never match, so they
/// are never prune candidates.
fn is_bundle_name(name: &str) -> bool {
    name.len() == 64 && name.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// C8: the bundle dirs that are stale: 64-hex entry names whose target
/// is neither the freshly staged hash nor the `previous` symlink target.
/// `entries` are the data-dir entry names, `keep` the paths whose
/// `file_name` must survive; the result holds the relative entry names
/// for the caller to join onto the data dir and remove. Non-hex names are
/// ignored outright (never a candidate, whatever `keep` says).
fn stale_bundles(entries: &[String], keep: &[PathBuf]) -> Vec<PathBuf> {
    entries
        .iter()
        .filter(|name| is_bundle_name(name) && !keep.iter().any(|k| kept(k, name)))
        .map(PathBuf::from)
        .collect()
}

/// C8: `true` when `keep` names `name` (compares file names only, so an
/// absolute or relative symlink target both work).
fn kept(keep: &Path, name: &str) -> bool {
    keep.file_name() == Some(std::ffi::OsStr::new(name))
}

/// C8: remove the bundles orphaned by a `current`/`previous` rotation.
/// Kept: the new hash (`current`) and the `previous` target (the one
/// rollback bundle k3s parity keeps around). Everything else that looks
/// like a bundle goes; every error is logged and ignored, so staging must
/// not fail because a prune did.
fn prune_stale_bundles(data_dir: &Path, new_hash: &str) {
    let entries: Vec<String> = match fs::read_dir(data_dir) {
        Ok(rd) => rd
            .flatten()
            .filter_map(|e| e.file_name().into_string().ok())
            .collect(),
        Err(_) => return,
    };
    // `previous` may be absent on a first stage: an empty path keeps
    // nothing extra (its file_name() is None).
    let previous = fs::read_link(data_dir.join("previous")).unwrap_or_default();
    let keep = vec![PathBuf::from(new_hash), previous];
    for stale in stale_bundles(&entries, &keep) {
        let path = data_dir.join(&stale);
        match fs::remove_dir_all(&path) {
            Ok(()) => tracing::info!(
                target: "init-pro",
                bundle = %stale.display(),
                "pruned stale staged bundle (C8)"
            ),
            Err(e) => tracing::debug!(
                target: "init-pro",
                bundle = %stale.display(),
                error = %e,
                "stale staged bundle not pruned (C8)"
            ),
        }
    }
}

/// Files under `bin/` that aren't text artifacts are executables.
fn is_executable(logical: &str) -> bool {
    let lower = logical.to_ascii_lowercase();
    if !lower.starts_with("bin/") {
        return false;
    }
    !lower.ends_with(".md")
        && !lower.ends_with(".txt")
        && !lower.ends_with("license")
        && !lower.ends_with("notice")
        && !lower.ends_with("readme")
        && !lower.ends_with("readme.md")
}

/// PATH entries for child processes: CNI aux dir first, then bin.
fn path_entries(current: &Path) -> Vec<PathBuf> {
    vec![current.join("bin").join("aux"), current.join("bin")]
}

/// Encode bytes as lowercase hex.
fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Staging failure.
#[derive(Debug)]
pub struct StageError {
    kind: StageErrorKind,
}

#[derive(Debug)]
enum StageErrorKind {
    Io {
        ctx: String,
        source: std::io::Error,
    },
    NoAssets,
    SizeMismatch {
        path: String,
        expected: u64,
        actual: u64,
    },
    HashMismatch {
        path: String,
        expected: String,
        actual: String,
    },
}

impl StageError {
    fn io(ctx: &str, source: std::io::Error) -> Self {
        StageError {
            kind: StageErrorKind::Io {
                ctx: ctx.into(),
                source,
            },
        }
    }
    fn no_assets() -> Self {
        StageError {
            kind: StageErrorKind::NoAssets,
        }
    }
    fn size_mismatch(path: &str, expected: u64, actual: u64) -> Self {
        StageError {
            kind: StageErrorKind::SizeMismatch {
                path: path.into(),
                expected,
                actual,
            },
        }
    }
    fn hash_mismatch(path: &str, expected: &str, actual: &str) -> Self {
        StageError {
            kind: StageErrorKind::HashMismatch {
                path: path.into(),
                expected: expected.into(),
                actual: actual.into(),
            },
        }
    }
}

impl std::fmt::Display for StageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.kind {
            StageErrorKind::Io { ctx, source } => write!(f, "{ctx}: {source}"),
            StageErrorKind::NoAssets => write!(
                f,
                "no embedded assets to stage (build with INIT_PRO_EMBED=1)"
            ),
            StageErrorKind::SizeMismatch {
                path,
                expected,
                actual,
            } => {
                write!(
                    f,
                    "size mismatch for {path}: expected {expected}, got {actual}"
                )
            }
            StageErrorKind::HashMismatch {
                path,
                expected,
                actual,
            } => {
                write!(
                    f,
                    "SHA-256 mismatch for {path}: expected {expected}, got {actual}"
                )
            }
        }
    }
}
impl std::error::Error for StageError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compute_data_hash_is_deterministic() {
        let a = compute_data_hash("abc  bin/x\n");
        let b = compute_data_hash("abc  bin/x\n");
        assert_eq!(a, b);
        assert_ne!(a, compute_data_hash("abc  bin/y\n"));
    }

    #[test]
    fn is_executable_detects_binaries() {
        assert!(is_executable("bin/runc"));
        assert!(is_executable("bin/aux/bridge"));
        assert!(!is_executable("bin/aux/LICENSE"));
        assert!(!is_executable("bin/aux/README.md"));
        assert!(!is_executable("bin/aux/README"));
        assert!(!is_executable("etc/config.yaml"));
    }

    #[test]
    fn path_entries_order() {
        let entries = path_entries(Path::new("/dd/data/current"));
        assert_eq!(entries.len(), 2);
        assert!(entries[0].to_string_lossy().ends_with("bin/aux"));
        assert!(entries[1].to_string_lossy().ends_with("bin"));
    }

    #[test]
    fn hex_encode_lowercase() {
        assert_eq!(hex_encode(&[0xab, 0xcd]), "abcd");
        assert_eq!(hex_encode(&[0x00, 0xff]), "00ff");
    }

    #[test]
    fn fast_path_misses_when_no_symlink() {
        assert!(!fast_path_hit(Path::new("/nonexistent/current"), "abc"));
    }

    #[test]
    fn c8_stale_bundles_keeps_new_and_previous_only() {
        // C8: only 64-hex names are candidates; the fresh hash and the
        // `previous` target survive, everything else is pruned.
        let new = "a".repeat(64);
        let prev = "b".repeat(64);
        let orphan = "c".repeat(64);
        let entries = vec![
            new.clone(),
            prev.clone(),
            orphan.clone(),
            "current".to_string(),
            "previous".to_string(),
            ".lock".to_string(),
            format!("{}-tmp", orphan),
            "not-a-bundle".to_string(),
            // Uppercase / short hex is not a bundle name.
            "C".repeat(64),
            "dead".to_string(),
        ];
        let keep = vec![PathBuf::from(&new), PathBuf::from(&prev)];
        let stale = stale_bundles(&entries, &keep);
        assert_eq!(stale, vec![PathBuf::from(&orphan)]);
    }

    #[test]
    fn c8_stale_bundles_without_a_previous_keeps_only_the_new_hash() {
        // C8: first stage (no `previous` yet) still prunes older orphans.
        let new = "1".repeat(64);
        let entries = vec![new.clone(), "2".repeat(64)];
        assert_eq!(
            stale_bundles(&entries, &[PathBuf::from(&new), PathBuf::new()]),
            vec![PathBuf::from("2".repeat(64))]
        );
        // Nothing kept (defensive) -> every bundle is stale.
        assert_eq!(
            stale_bundles(&entries, &[]).len(),
            2,
            "with an empty keep list every hex bundle is stale"
        );
    }

    #[test]
    fn c8_prune_stale_bundles_removes_orphans_and_spares_live_ones() {
        // C8: end-to-end over a temp data dir: the new hash dir and the
        // `previous` target stay, the orphaned bundle goes, and non-bundle
        // entries (current/previous links, tmp dir) are untouched.
        let dd = std::env::temp_dir().join(format!("init-pro-stage-c8-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dd);
        fs::create_dir_all(&dd).unwrap();
        let new = "a".repeat(64);
        let prev = "b".repeat(64);
        let orphan = "c".repeat(64);
        for name in [&new, &prev, &orphan] {
            fs::create_dir_all(dd.join(name)).unwrap();
            fs::write(dd.join(name).join("bin"), b"x").unwrap();
        }
        fs::create_dir_all(dd.join("current")).unwrap();
        std::os::unix::fs::symlink(&prev, dd.join("previous")).unwrap();

        prune_stale_bundles(&dd, &new);

        assert!(dd.join(&new).is_dir(), "the freshly staged bundle stays");
        assert!(dd.join(&prev).is_dir(), "the previous target stays");
        assert!(!dd.join(&orphan).exists(), "the orphaned bundle is pruned");
        assert!(dd.join("current").is_dir(), "non-hex entries are ignored");

        // Idempotent: a second prune finds nothing left to remove.
        prune_stale_bundles(&dd, &new);
        assert!(dd.join(&new).is_dir() && dd.join(&prev).is_dir());
        let _ = fs::remove_dir_all(&dd);
    }
}
