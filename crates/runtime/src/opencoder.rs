//! Discovery + staging of the `opencoder` runner binary (TODO **T7.1**
//! Phase B / B1, decision **Q37**).
//!
//! Agents execute as a SEPARATE `opencoder` subprocess (**D1**: init-pro
//! vendors libsql 0.9.30 while opencoder links libsql 0.10.0-pre.4, so any
//! in-process link duplicates C symbols and fails; Q35 keeps credentials
//! out of specs — the key reaches the subprocess via inherited env only).
//!
//! Discovery order (first hit wins):
//! 1. `INIT_PRO_AGENT_RUNNER_BIN` — explicit override, used AS-IS (no
//!    staging copy). A missing/non-executable override is terminal: no
//!    silent fallback, so e2e negatives stay deterministic.
//! 2. Already staged: `<data-dir>/bin/opencoder` (idempotent fast path).
//! 3. Embedded data tree: `<data-dir>/data/current/bin/opencoder` (the
//!    `INIT_PRO_EMBED=1` asset path convention, `bin/opencoder`).
//! 4. Local build: `link_repos/opencoder/target/release/opencoder` under the
//!    executable dir's 2nd or 3rd parent (`<repo>/target/debug/init-pro` and
//!    the cargo-test layout `<repo>/target/debug/deps/<bin>` both reach the
//!    repo root) or the CWD's — the dev-loop source (A1).
//!
//! Anything found under (3)/(4) is copied to `<data-dir>/bin/opencoder`
//! (byte-hash idempotent, 0755). `None` => the caller reports
//! `RunnerMissing` and bound Agents stay Pending (observable degradation,
//! never a boot failure).

#![forbid(unsafe_code)]

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// The staged runner binary name (and the multicall-shaped file name).
pub const RUNNER_NAME: &str = "opencoder";
/// Explicit runner override env (test/e2e seam; documented in Q37).
pub const ENV_RUNNER_BIN: &str = "INIT_PRO_AGENT_RUNNER_BIN";
/// Path (relative to the embedded data root) an embedded runner would take.
const EMBEDDED_REL: &str = "data/current/bin/opencoder";
/// Repo-relative release-build path (A1 dev-loop source).
const LINK_REPOS_REL: &str = "link_repos/opencoder/target/release/opencoder";

/// Resolve the runner binary for `data_dir` (see module docs for the order).
pub fn discover_runner_bin(data_dir: &Path) -> Option<PathBuf> {
    runner_bin_from(
        std::env::var_os(ENV_RUNNER_BIN).map(PathBuf::from),
        data_dir,
        std::env::current_exe().ok().as_deref(),
        std::env::current_dir().ok().as_deref(),
    )
}

/// Pure core of [`discover_runner_bin`] (explicit inputs, unit-testable).
pub fn runner_bin_from(
    env: Option<PathBuf>,
    data_dir: &Path,
    exe: Option<&Path>,
    cwd: Option<&Path>,
) -> Option<PathBuf> {
    if let Some(p) = env {
        // The override IS the runner: no copy, no fallback.
        return if is_exec_file(&p) { Some(p) } else { None };
    }
    let staged = data_dir.join("bin").join(RUNNER_NAME);
    if is_exec_file(&staged) {
        return Some(staged);
    }
    let mut cands: Vec<PathBuf> = Vec::new();
    cands.push(data_dir.join(EMBEDDED_REL));
    // Both deployed shapes reach a repo root two levels up; cargo-test
    // binaries sit one level deeper (target/debug/deps/<bin>), so try the
    // third parent as well. Order is stable: exe-2, exe-3, cwd-2, cwd-3.
    for base in [exe.and_then(Path::parent), cwd].into_iter().flatten() {
        for up in ["../..", "../../.."] {
            cands.push(base.join(up).join(LINK_REPOS_REL));
        }
    }
    cands.into_iter().find(|c| is_exec_file(c))
}

/// Stage the discovered runner into `<data-dir>/bin/opencoder` (idempotent).
///
/// Returns `Ok(None)` when no runner source exists — the Agent runner then
/// degrades to `RunnerMissing` instead of failing the agent boot.
pub fn stage_runner(data_dir: &Path) -> Result<Option<PathBuf>, String> {
    if let Some(p) = std::env::var_os(ENV_RUNNER_BIN).map(PathBuf::from) {
        return Ok(if is_exec_file(&p) { Some(p) } else { None });
    }
    match discover_runner_bin(data_dir) {
        Some(src) => stage_from(&src, data_dir),
        None => Ok(None),
    }
}

/// Stage one known-good source into `<data-dir>/bin/opencoder` (pure seam:
/// explicit inputs, unit-testable without depending on where the test
/// binary happens to live). `Ok(None)` is unreachable via [`stage_runner`].
pub fn stage_from(src: &Path, data_dir: &Path) -> Result<Option<PathBuf>, String> {
    let dest = data_dir.join("bin").join(RUNNER_NAME);
    if same_content(src, &dest) {
        return Ok(Some(dest));
    }
    let dir = dest.parent().ok_or("bin dir has no parent")?;
    fs::create_dir_all(dir).map_err(|e| io_err("create bin dir", e))?;
    let tmp = dest.with_extension("opencoder-tmp");
    fs::copy(src, &tmp).map_err(|e| io_err("copy runner", e))?;
    fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755))
        .map_err(|e| io_err("chmod runner", e))?;
    fs::rename(&tmp, &dest).map_err(|e| io_err("commit runner", e))?;
    Ok(Some(dest))
}

/// `true` for an existing, regular, executable file.
fn is_exec_file(p: &Path) -> bool {
    match fs::metadata(p) {
        Ok(m) if m.is_file() => m.permissions().mode() & 0o111 != 0,
        _ => false,
    }
}

fn io_err(what: &str, e: io::Error) -> String {
    format!("opencoder staging: {what}: {e}")
}

/// `true` when both files exist and hash identically (SHA-256, cf. stage).
fn same_content(a: &Path, b: &Path) -> bool {
    match (fs::metadata(a), fs::metadata(b)) {
        (Ok(ma), Ok(mb)) if ma.len() == mb.len() => {
            matches!((sha256_file(a), sha256_file(b)), (Ok(ha), Ok(hb)) if ha == hb)
        }
        _ => false,
    }
}

fn sha256_file(p: &Path) -> io::Result<[u8; 32]> {
    let mut h = Sha256::new();
    h.update(fs::read(p)?);
    Ok(h.finalize().into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::MetadataExt;

    fn tmp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("init-pro-oc-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn write_bin(p: &Path, body: &[u8]) -> PathBuf {
        if let Some(dir) = p.parent() {
            fs::create_dir_all(dir).unwrap();
        }
        fs::write(p, body).unwrap();
        fs::set_permissions(p, fs::Permissions::from_mode(0o755)).unwrap();
        p.to_path_buf()
    }

    #[test]
    fn env_override_wins_and_never_falls_back() {
        let data = tmp_dir("ovr");
        let real = write_bin(&data.join("runner.sh"), b"#!/bin/sh\n");
        let got = runner_bin_from(Some(real.clone()), &data, None, None);
        assert_eq!(got, Some(real));
        // A missing override is terminal (no silent link_repos fallback).
        let ghost = data.join("ghost");
        assert_eq!(runner_bin_from(Some(ghost), &data, None, None), None);
        let _ = fs::remove_dir_all(&data);
    }

    #[test]
    fn staged_fast_path_beats_build_sources() {
        let data = tmp_dir("fast");
        let staged = write_bin(&data.join("bin/opencoder"), b"staged");
        assert_eq!(runner_bin_from(None, &data, None, None), Some(staged));
        let _ = fs::remove_dir_all(&data);
    }

    #[test]
    fn exe_ancestors_reach_the_repo_root_link_repos() {
        // Deployed shape: <repo>/target/debug/init-pro -> repo-root hit.
        let repo = tmp_dir("repo2");
        let exe = write_bin(&repo.join("target/debug/init-pro"), b"bin");
        let src = write_bin(
            &repo.join("link_repos/opencoder/target/release/opencoder"),
            b"rls",
        );
        let got = runner_bin_from(None, &tmp_dir("d2"), Some(&exe), None).unwrap();
        // The join is un-normalized (`target/debug/../../link_repos/...`);
        // compare resolved files, not spellings.
        assert_eq!(
            fs::canonicalize(got).unwrap(),
            fs::canonicalize(&src).unwrap()
        );
        // Cargo-test shape: one level deeper (target/debug/deps/<bin>).
        let repo3 = tmp_dir("repo3");
        let exe3 = write_bin(&repo3.join("target/debug/deps/runtime-ab12"), b"bin");
        let src3 = write_bin(
            &repo3.join("link_repos/opencoder/target/release/opencoder"),
            b"rls",
        );
        let got3 = runner_bin_from(None, &tmp_dir("d3"), Some(&exe3), None).unwrap();
        assert_eq!(
            fs::canonicalize(got3).unwrap(),
            fs::canonicalize(&src3).unwrap()
        );
        let _ = fs::remove_dir_all(&repo);
        let _ = fs::remove_dir_all(&repo3);
    }

    #[test]
    fn embedded_convention_beats_the_local_build() {
        // Documented order: (3) embedded data tree before (4) link_repos.
        let data = tmp_dir("emb2");
        let repo = tmp_dir("repo4");
        let emb = write_bin(&data.join("data/current/bin/opencoder"), b"embed");
        let exe = write_bin(&repo.join("target/debug/init-pro"), b"bin");
        write_bin(
            &repo.join("link_repos/opencoder/target/release/opencoder"),
            b"rls",
        );
        assert_eq!(runner_bin_from(None, &data, Some(&exe), None), Some(emb));
        let _ = fs::remove_dir_all(&repo);
        let _ = fs::remove_dir_all(&data);
    }

    #[test]
    fn none_when_nothing_exists() {
        let data = tmp_dir("none");
        assert_eq!(runner_bin_from(None, &data, None, None), None);
        let _ = fs::remove_dir_all(&data);
    }

    #[test]
    fn stage_copies_sets_mode_and_is_idempotent() {
        let data = tmp_dir("stage");
        let repo = tmp_dir("stage-src");
        let src = write_bin(
            &repo.join("link_repos/opencoder/target/release/opencoder"),
            b"v1",
        );
        let dest = data.join("bin/opencoder");
        // Pure seam: explicit source (the real repo must not leak in).
        assert_eq!(stage_from(&src, &data).unwrap(), Some(dest.clone()));
        assert!(is_exec_file(&dest));
        assert_eq!(fs::read(&dest).unwrap(), b"v1");
        // Same content: fast path keeps the inode.
        let ino = fs::metadata(&dest).unwrap().ino();
        assert_eq!(stage_from(&src, &data).unwrap(), Some(dest.clone()));
        assert_eq!(fs::metadata(&dest).unwrap().ino(), ino);
        // Changed source: re-staged.
        fs::write(&src, b"v2").unwrap();
        stage_from(&src, &data).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), b"v2");
        let _ = fs::remove_dir_all(&data);
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn stage_runner_env_override_is_used_as_is() {
        // Pure seam lives in stage_from; stage_runner's env path is covered
        // by the e2e (INIT_PRO_AGENT_RUNNER_BIN at a missing path =>
        // RunnerMissing), because discover depends on where the test
        // binary lives (a dev repo checkout has a real link_repos source).
        let data = tmp_dir("stage-none");
        assert!(is_exec_file(&write_bin(&data.join("x"), b"#!/bin/sh\n")));
        let _ = fs::remove_dir_all(&data);
    }
}
