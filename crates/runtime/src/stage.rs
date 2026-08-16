//! Idempotent staging of the bundled runtime tree (TODO **T4.1**, Q25).
//!
//! Copies the vendored containerd bundle (Q24 layout: containerd + shims +
//! ctr + crictl + runc + `aux/` CNI plugins) k3s-style into
//! `<data-dir>/agent/containerd/`. Staging is content-addressed (SHA-256),
//! so repeated agent boots are no-ops unless the vendor tree changed.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// Files staged beside `containerd` (it resolves shims relative to its own
/// executable directory; runc/aux feed the v2 runtime and CNI).
pub const RUNTIME_FILES: &[&str] = &[
    "containerd",
    "containerd-shim",
    "containerd-shim-runc-v1",
    "containerd-shim-runc-v2",
    "ctr",
    "crictl",
    "runc",
];

/// Loopback-only CNI conf for v1 wiring (flannel lands with T4.3).
pub const CNI_CONF_NAME: &str = "10-init-pro.conflist";
/// The CNI network configuration written into `cni/net.d`.
pub const CNI_CONF: &str =
    r#"{"cniVersion":"1.0.0","name":"init-pro","plugins":[{"type":"loopback"}]}"#;

/// What one staging pass did (logged by the agent bootstrap).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StageOutcome {
    /// Files freshly copied (content differed or dest absent).
    pub copied: Vec<String>,
    /// Files already staged with identical content.
    pub skipped: Vec<String>,
    /// Optional files absent from the vendor tree (e.g. `crictl` before the
    /// pin is fetched — non-fatal, the supervisor only needs `containerd`).
    pub missing: Vec<String>,
}

impl StageOutcome {
    /// Nothing copied and nothing missing: a pure no-op pass.
    pub fn is_noop(&self) -> bool {
        self.copied.is_empty() && self.missing.is_empty()
    }
}

/// Locate the vendor bundle at runtime: `INIT_PRO_VENDOR_BIN` override, else
/// the repo layout relative to the running executable (`<exe>/../../vendor/bin`,
/// e.g. `target/debug/init-pro`), else `./vendor/bin`. `None` when no usable
/// bundle is found (embedded assets arrive with the T4.2+ deploy story).
pub fn vendor_bin_root() -> Option<PathBuf> {
    let env = std::env::var_os("INIT_PRO_VENDOR_BIN").map(PathBuf::from);
    let exe = std::env::current_exe().ok();
    let cwd = std::env::current_dir().ok();
    vendor_bin_root_from(env.as_deref(), exe.as_deref(), cwd.as_deref())
}

/// Pure core of [`vendor_bin_root`] (explicit inputs, unit-testable).
fn vendor_bin_root_from(
    env: Option<&Path>,
    exe: Option<&Path>,
    cwd: Option<&Path>,
) -> Option<PathBuf> {
    if let Some(v) = env {
        if v.join("containerd").is_file() {
            return Some(v.to_path_buf());
        }
    }
    let mut cands: Vec<PathBuf> = Vec::new();
    if let Some(dir) = exe.and_then(|e| e.parent()) {
        cands.push(dir.join("../../vendor/bin"));
        cands.push(dir.join("../vendor/bin"));
    }
    if let Some(cwd) = cwd {
        cands.push(cwd.join("vendor/bin"));
    }
    cands
        .into_iter()
        .map(|c| fs::canonicalize(&c).unwrap_or(c))
        .find(|c| c.join("containerd").is_file())
}

fn sha256_file(p: &Path) -> io::Result<[u8; 32]> {
    let mut h = Sha256::new();
    h.update(fs::read(p)?);
    Ok(h.finalize().into())
}

/// `true` when both files exist and hash identically.
fn same_content(a: &Path, b: &Path) -> bool {
    match (fs::metadata(a), fs::metadata(b)) {
        (Ok(ma), Ok(mb)) if ma.len() == mb.len() => {
            matches!(
                (sha256_file(a), sha256_file(b)),
                (Ok(ha), Ok(hb)) if ha == hb
            )
        }
        _ => false,
    }
}

/// Stage the runtime tree from `vendor` into `dest` (idempotent).
///
/// `containerd` itself must exist in `vendor`; every other entry is
/// optional (recorded in [`StageOutcome::missing`]).
pub fn stage_containerd_tree(vendor: &Path, dest: &Path) -> io::Result<StageOutcome> {
    if !vendor.join("containerd").is_file() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("vendor bundle has no containerd: {}", vendor.display()),
        ));
    }
    fs::create_dir_all(dest)?;
    let mut out = StageOutcome::default();
    for name in RUNTIME_FILES {
        let src = vendor.join(name);
        let dst = dest.join(name);
        if !src.is_file() {
            out.missing.push((*name).to_string());
            continue;
        }
        if same_content(&src, &dst) {
            out.skipped.push((*name).to_string());
            continue;
        }
        fs::copy(&src, &dst)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&dst, fs::Permissions::from_mode(0o755))?;
        }
        out.copied.push((*name).to_string());
    }
    stage_aux_dir(vendor, dest, &mut out)?;
    Ok(out)
}

/// Copy `vendor/aux` (CNI plugins) when present and not already staged.
fn stage_aux_dir(vendor: &Path, dest: &Path, out: &mut StageOutcome) -> io::Result<()> {
    let src = vendor.join("aux");
    if !src.is_dir() {
        return Ok(());
    }
    let dst = dest.join("aux");
    if dst.join("loopback").is_file() {
        return Ok(());
    }
    fs::create_dir_all(&dst)?;
    for entry in fs::read_dir(&src)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let to = dst.join(entry.file_name());
        fs::copy(entry.path(), &to)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&to, fs::Permissions::from_mode(0o755))?;
        }
    }
    out.copied.push("aux".to_string());
    Ok(())
}

/// Write the loopback CNI conf (idempotent). Returns `true` when written.
pub fn write_cni_conf(cni_conf_dir: &Path) -> io::Result<bool> {
    let path = cni_conf_dir.join(CNI_CONF_NAME);
    if path.is_file() {
        return Ok(false);
    }
    fs::create_dir_all(cni_conf_dir)?;
    fs::write(&path, CNI_CONF)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rt-stage-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn fake_vendor(dir: &Path, files: &[&str]) -> PathBuf {
        let v = dir.join("vendor").join("bin");
        fs::create_dir_all(v.join("aux")).unwrap();
        for f in files {
            fs::write(v.join(f), format!("payload-{f}").as_bytes()).unwrap();
        }
        fs::write(v.join("aux/loopback"), b"loopback-bin").unwrap();
        v
    }

    #[test]
    fn stages_all_files_and_is_idempotent() {
        let d = temp_dir("idem");
        let v = fake_vendor(&d, RUNTIME_FILES);
        let dest = d.join("agent/containerd");

        let first = stage_containerd_tree(&v, &dest).unwrap();
        assert_eq!(first.copied.len(), RUNTIME_FILES.len() + 1); // + aux
        assert!(first.missing.is_empty());
        assert!(dest.join("containerd").is_file());
        assert!(dest.join("aux/loopback").is_file());
        assert_eq!(
            fs::read_to_string(dest.join("crictl")).unwrap(),
            "payload-crictl"
        );

        // Second pass: identical content -> pure no-op (no copies, no mtimes touched).
        let second = stage_containerd_tree(&v, &dest).unwrap();
        assert!(
            second.is_noop(),
            "second pass should be a no-op: {second:?}"
        );
        assert_eq!(second.skipped.len(), RUNTIME_FILES.len());
    }

    #[test]
    fn restages_when_content_changes() {
        let d = temp_dir("restage");
        let v = fake_vendor(&d, &["containerd"]);
        let dest = d.join("agent/containerd");
        stage_containerd_tree(&v, &dest).unwrap();

        fs::write(v.join("containerd"), b"new-version").unwrap();
        let out = stage_containerd_tree(&v, &dest).unwrap();
        assert_eq!(out.copied, vec!["containerd".to_string()]);
        assert_eq!(
            fs::read_to_string(dest.join("containerd")).unwrap(),
            "new-version"
        );
    }

    #[test]
    fn optional_files_missing_are_recorded_not_fatal() {
        let d = temp_dir("optional");
        let v = fake_vendor(&d, &["containerd", "runc"]); // no ctr/crictl/shims
        let out = stage_containerd_tree(&v, &d.join("dest")).unwrap();
        assert!(out.copied.contains(&"containerd".to_string()));
        assert!(out.missing.contains(&"crictl".to_string()));
        assert!(!out.missing.contains(&"containerd".to_string()));
    }

    #[test]
    fn vendor_without_containerd_is_an_error() {
        let d = temp_dir("err");
        let v = fake_vendor(&d, &["runc"]);
        let err = stage_containerd_tree(&v, &d.join("dest")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn cni_conf_written_once() {
        let d = temp_dir("cni");
        let conf = d.join("cni/net.d");
        assert!(write_cni_conf(&conf).unwrap());
        assert!(!write_cni_conf(&conf).unwrap(), "second write is a no-op");
        let text = fs::read_to_string(conf.join(CNI_CONF_NAME)).unwrap();
        assert!(text.contains(r#""name":"init-pro""#));
        assert!(text.contains("loopback"));
        assert!(
            text.starts_with('{') && text.ends_with('}'),
            "valid JSON object"
        );
    }

    #[test]
    fn vendor_bin_root_env_override_wins_when_containerd_present() {
        let d = temp_dir("rootenv");
        let got =
            vendor_bin_root_from(Some(&fake_vendor(&d, &["containerd"])), None, None).unwrap();
        assert!(got.join("containerd").is_file());
    }

    #[test]
    fn vendor_bin_root_env_pointing_nowhere_falls_back_to_exe_layout() {
        let d = temp_dir("rootfallback");
        fake_vendor(&d, &["containerd"]);
        // Repo layout: <root>/vendor/bin + <root>/target/debug/init-pro, so
        // the exe dir resolves `../../vendor/bin` back onto the bundle. The
        // exe's parent must exist for path resolution (like a real target dir).
        let exe = d.join("target/debug/init-pro");
        fs::create_dir_all(exe.parent().unwrap()).unwrap();
        let got =
            vendor_bin_root_from(Some(Path::new("/nonexistent/vendor/bin")), Some(&exe), None)
                .expect("falls back to the exe-relative candidate");
        assert!(got.join("containerd").is_file());
    }

    #[test]
    fn vendor_bin_root_none_without_any_candidate() {
        let d = temp_dir("rootnone");
        let empty_exe = d.join("bin/tool");
        assert!(vendor_bin_root_from(None, Some(&empty_exe), Some(&d)).is_none());
    }
}
