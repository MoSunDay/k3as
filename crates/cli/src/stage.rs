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

use std::fs::File;
use common::embed::EmbeddedAsset;
use infra::Config;
use sha2::{Digest, Sha256};

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
pub fn stage(cfg: &Config, assets: &[EmbeddedAsset], sha256_sums: &str) -> Result<StageResult, StageError> {
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
    lock.lock()
        .map_err(|e| StageError::io("flock", e))?;

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

    let decompressed = zstd::decode_all(asset.zstd)
        .map_err(|e| StageError::io("zstd decompress", e))?;

    if decompressed.len() as u64 != asset.size {
        return Err(StageError::size_mismatch(asset.path, asset.size, decompressed.len() as u64));
    }

    let mut f = fs::File::create(&dest).map_err(|e| StageError::io("create staged file", e))?;
    f.write_all(&decompressed).map_err(|e| StageError::io("write staged file", e))?;
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
    vec![
        current.join("bin").join("aux"),
        current.join("bin"),
    ]
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
    Io { ctx: String, source: std::io::Error },
    NoAssets,
    SizeMismatch { path: String, expected: u64, actual: u64 },
    HashMismatch { path: String, expected: String, actual: String },
}

impl StageError {
    fn io(ctx: &str, source: std::io::Error) -> Self {
        StageError { kind: StageErrorKind::Io { ctx: ctx.into(), source } }
    }
    fn no_assets() -> Self {
        StageError { kind: StageErrorKind::NoAssets }
    }
    fn size_mismatch(path: &str, expected: u64, actual: u64) -> Self {
        StageError { kind: StageErrorKind::SizeMismatch { path: path.into(), expected, actual } }
    }
    fn hash_mismatch(path: &str, expected: &str, actual: &str) -> Self {
        StageError { kind: StageErrorKind::HashMismatch { path: path.into(), expected: expected.into(), actual: actual.into() } }
    }
}

impl std::fmt::Display for StageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.kind {
            StageErrorKind::Io { ctx, source } => write!(f, "{ctx}: {source}"),
            StageErrorKind::NoAssets => write!(f, "no embedded assets to stage (build with INIT_PRO_EMBED=1)"),
            StageErrorKind::SizeMismatch { path, expected, actual } => {
                write!(f, "size mismatch for {path}: expected {expected}, got {actual}")
            }
            StageErrorKind::HashMismatch { path, expected, actual } => {
                write!(f, "SHA-256 mismatch for {path}: expected {expected}, got {actual}")
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
}
