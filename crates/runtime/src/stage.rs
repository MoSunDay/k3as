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

/// CNI conf written into `cni/net.d` (T4.3 Scope A, Q34: the flannel
/// meta-plugin chain, replacing the pre-T4.3 static `10-init-pro` one).
pub const CNI_CONF_NAME: &str = "10-flannel.conflist";
/// The CNI network configuration written into `cni/net.d`.
///
/// The flannel meta-plugin builds the real `bridge` + `host-local` delegate
/// from flanneld's `subnet.env` at sandbox time, so the pod IP lands in the
/// node's allocated `/24` (`spec.podCIDR`) instead of a fixed subnet —
/// containerd 1.7 CRI still hard-fails `RunPodSandbox` unless the CNI
/// result carries an IP on `eth0`. Three port notes:
/// - the Rust flannel NetConf has **no** `subnetFile` JSON key; the plugin
///   resolves the env file through `FLANNEL_SUBNET_FILE`, which
///   [`start_agent_runtime`](crate::start_agent_runtime) injects into the
///   containerd child environment (plugins inherit containerd's env);
/// - **no `isDefaultGateway`**: the flannel plugin already injects
///   `ipam.routes = ["0.0.0.0/0"]` into the `host-local` delegate
///   (`flannel-cni::netconf::build_delegate_conf`), and the vendored
///   cni-plugins `bridge` v1.5.1 programs `isDefaultGateway` routes
///   unconditionally — both together double-add the default route and
///   `RunPodSandbox` dies with `failed to add route ... file exists`
///   (code 999). The injected ipam route alone gives pods the default
///   route (verified against the vendored bundle);
/// - it delegates to `bridge`/`host-local` binaries under the CNI `bin_dir`
///   (`<runtime>/aux`), guaranteed present via [`ensure_cni_plugins`].
pub const CNI_CONF: &str = r#"{"name":"flannel","cniVersion":"1.0.0","plugins":[{"type":"flannel","delegate":{"hairpinMode":true}}]}"#;

/// CNI plugins the flannel delegate chain needs (extracted from the vendor
/// cache tgz when `aux/` was not staged by the vendor acquire step).
pub const CNI_PLUGIN_FILES: &[&str] = &["bridge", "host-local", "loopback"];
/// Repo-relative location of the vendored cni-plugins tarball
/// (vendor/versions.toml pin v1.5.1).
pub const CNI_PLUGINS_TGZ: &str = "cni-plugins-linux-amd64-v1.5.1.tgz";
/// Inside the tgz, binaries live under `bin/`.
pub const CNI_TGZ_PREFIX: &str = "bin/";

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

/// Write the CNI conf (idempotent). Returns `true` when written.
pub fn write_cni_conf(cni_conf_dir: &Path) -> io::Result<bool> {
    let path = cni_conf_dir.join(CNI_CONF_NAME);
    if path.is_file() {
        return Ok(false);
    }
    fs::create_dir_all(cni_conf_dir)?;
    fs::write(&path, CNI_CONF)?;
    Ok(true)
}

/// Locate the vendor cache (the pinned upstream tarballs, T0.2 layout) with
/// the same resolution order as [`vendor_bin_root`]: `INIT_PRO_VENDOR_CACHE`
/// override, else exe-relative repo layout, else cwd.
pub fn vendor_cache_dir() -> Option<PathBuf> {
    let env = std::env::var_os("INIT_PRO_VENDOR_CACHE").map(PathBuf::from);
    let exe = std::env::current_exe().ok();
    let cwd = std::env::current_dir().ok();
    vendor_cache_dir_from(env.as_deref(), exe.as_deref(), cwd.as_deref())
}

/// Pure core of [`vendor_cache_dir`] (explicit inputs, unit-testable).
fn vendor_cache_dir_from(
    env: Option<&Path>,
    exe: Option<&Path>,
    cwd: Option<&Path>,
) -> Option<PathBuf> {
    if let Some(v) = env {
        if v.join(CNI_PLUGINS_TGZ).is_file() {
            return Some(v.to_path_buf());
        }
    }
    let mut cands: Vec<PathBuf> = Vec::new();
    if let Some(dir) = exe.and_then(|e| e.parent()) {
        cands.push(dir.join("../../vendor/cache"));
        cands.push(dir.join("../vendor/cache"));
    }
    if let Some(cwd) = cwd {
        cands.push(cwd.join("vendor/cache"));
    }
    cands
        .into_iter()
        .map(|c| fs::canonicalize(&c).unwrap_or(c))
        .find(|c| c.join(CNI_PLUGINS_TGZ).is_file())
}

/// Guarantee the flannel delegate chain's CNI plugins exist under
/// `<runtime_dir>/aux` (T4.3 Scope A). Missing binaries are extracted
/// from the vendored cni-plugins tarball via `tar -xOzf <tgz> bin/<name>`
/// (repo precedent: vendor acquire streams archives through `tar`) —
/// a single-member extraction to stdout, no directory churn. Returns the
/// names freshly extracted; an empty result means everything was present.
///
/// Errors when a plugin is missing AND the vendor cache has no tarball:
/// the caller logs + degrades (pods would fail CNI ADD, flannel itself
/// still runs).
pub fn ensure_cni_plugins(runtime_dir: &Path) -> io::Result<Vec<String>> {
    ensure_cni_plugins_from_cache(runtime_dir, vendor_cache_dir().as_deref())
}

/// Pure core of [`ensure_cni_plugins`] (explicit cache, unit-testable
/// without process-global env mutation, which `#![forbid(unsafe_code)]`
/// rules out in tests).
pub fn ensure_cni_plugins_from_cache(
    runtime_dir: &Path,
    cache: Option<&Path>,
) -> io::Result<Vec<String>> {
    let aux = runtime_dir.join("aux");
    fs::create_dir_all(&aux)?;
    let mut extracted = Vec::new();
    for name in CNI_PLUGIN_FILES {
        let dst = aux.join(name);
        if dst.is_file() {
            continue;
        }
        let Some(cache) = cache else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "CNI plugin {name} missing from {} and no vendor cache tarball \
                     ({CNI_PLUGINS_TGZ}) available",
                    aux.display()
                ),
            ));
        };
        let tgz = cache.join(CNI_PLUGINS_TGZ);
        let out = std::process::Command::new("tar")
            .arg("-xOzf")
            .arg(&tgz)
            .arg(format!("{CNI_TGZ_PREFIX}{name}"))
            .output()?;
        if !out.status.success() || out.stdout.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "extracting {CNI_TGZ_PREFIX}{name} from {} failed: {}",
                    tgz.display(),
                    String::from_utf8_lossy(&out.stderr).trim()
                ),
            ));
        }
        fs::write(&dst, &out.stdout)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&dst, fs::Permissions::from_mode(0o755))?;
        }
        extracted.push((*name).to_string());
    }
    Ok(extracted)
}

/// Stage the `flannel` CNI meta-plugin as a multicall alias (Q1): a symlink
/// from `<runtime_dir>/aux/flannel` back to this very binary. The flannel
/// meta-plugin must exist under the CNI `bin_dir` for containerd's delegate
/// chain to find it; as a symlink it re-execs `init-pro` whose argv[0]
/// dispatch routes to the embedded `flannel_cni::skel::run()`.
/// Returns `true` when the symlink was (re)created.
pub fn ensure_flannel_alias(runtime_dir: &Path) -> io::Result<bool> {
    let Ok(exe) = std::env::current_exe() else {
        tracing::warn!(
            target: "init-pro",
            "no current_exe; flannel CNI alias not staged (pods will fail CNI ADD)"
        );
        return Ok(false);
    };
    let aux = runtime_dir.join("aux");
    fs::create_dir_all(&aux)?;
    let link = aux.join("flannel");
    // Idempotent: correct symlink -> skip.
    if fs::read_link(&link)
        .ok()
        .and_then(|p| fs::canonicalize(&p).ok())
        .is_some_and(|resolved| resolved == fs::canonicalize(&exe).unwrap_or(exe.clone()))
    {
        return Ok(false);
    }
    let _ = fs::remove_file(&link);
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&exe, &link)?;
    }
    #[cfg(not(unix))]
    {
        fs::copy(&exe, &link)?;
    }
    tracing::info!(
        target: "init-pro",
        alias = %link.display(),
        "staged flannel CNI meta-plugin alias (multicall, Q1)"
    );
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
        // T4.3 Scope A: the flannel meta-plugin chain (Q34). No static
        // bridge/ipam block anymore — the delegate is built from
        // flanneld's subnet.env at sandbox time; the env path rides on
        // containerd's FLANNEL_SUBNET_FILE (Rust NetConf has no
        // `subnetFile` JSON key).
        assert!(text.contains(r#""name":"flannel""#));
        assert!(text.contains(r#""type":"flannel""#));
        assert!(text.contains("hairpinMode"));
        assert!(!text.contains("subnetFile"), "env-carried, not JSON");
        assert!(
            text.starts_with('{') && text.ends_with('}'),
            "valid JSON object"
        );
    }

    #[test]
    fn vendor_cache_dir_prefers_env_override() {
        let d = temp_dir("cacheenv");
        let cache = d.join("cache");
        fs::create_dir_all(&cache).unwrap();
        fs::write(cache.join(CNI_PLUGINS_TGZ), b"tgz").unwrap();
        let got = vendor_cache_dir_from(Some(&cache), None, None).unwrap();
        assert!(got.join(CNI_PLUGINS_TGZ).is_file());
        // A cache without the tarball is not a candidate.
        assert!(vendor_cache_dir_from(Some(&d.join("nope")), None, None).is_none());
    }

    #[test]
    fn vendor_cache_dir_falls_back_to_exe_layout() {
        let d = temp_dir("cachefallback");
        let cache = d.join("vendor/cache");
        fs::create_dir_all(&cache).unwrap();
        fs::write(cache.join(CNI_PLUGINS_TGZ), b"tgz").unwrap();
        let exe = d.join("target/debug/init-pro");
        fs::create_dir_all(exe.parent().unwrap()).unwrap();
        let got =
            vendor_cache_dir_from(None, Some(&exe), None).expect("exe-relative candidate");
        assert!(got.join(CNI_PLUGINS_TGZ).is_file());
    }

    #[test]
    fn ensure_cni_plugins_extracts_from_vendor_cache() {
        // Build a real tarball with `bin/bridge` + `bin/host-local` members
        // (the repo already relies on `tar` for vendor acquire), point the
        // cache env override at it, then extract into an empty aux dir.
        let d = temp_dir("plugins");
        let cache = d.join("cache");
        fs::create_dir_all(&cache).unwrap();
        let payload = d.join("payload");
        let bin = payload.join("bin");
        fs::create_dir_all(&bin).unwrap();
        for name in ["bridge", "host-local", "loopback"] {
            fs::write(bin.join(name), format!("#!bin/{name}\n")).unwrap();
        }
        let tgz = cache.join(CNI_PLUGINS_TGZ);
        let ok = std::process::Command::new("tar")
            .args(["-czf"])
            .arg(&tgz)
            .arg("-C")
            .arg(&payload)
            .args(["bin/bridge", "bin/host-local", "bin/loopback"])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            // No `tar` in the environment: the extraction guarantee is
            // env-dependent (same as vendor acquire), skip rather than fail.
            return;
        }
        let runtime = d.join("runtime");
        let got = ensure_cni_plugins_from_cache(&runtime, Some(&cache)).unwrap();
        assert_eq!(got, ["bridge", "host-local", "loopback"]);
        for name in CNI_PLUGIN_FILES {
            let p = runtime.join("aux").join(name);
            assert!(p.is_file(), "{name} extracted");
        }
        // Second pass: everything present, no extraction.
        assert!(ensure_cni_plugins_from_cache(&runtime, Some(&cache))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn ensure_cni_plugins_errors_without_a_cache_tarball() {
        let d = temp_dir("plugins-nocache");
        let runtime = d.join("runtime");
        let err = ensure_cni_plugins_from_cache(&runtime, None).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn ensure_flannel_alias_symlinks_current_exe() {
        let d = temp_dir("alias");
        let runtime = d.join("runtime");
        let first = ensure_flannel_alias(&runtime).unwrap();
        let link = runtime.join("aux/flannel");
        if first {
            assert!(link.exists(), "alias staged");
            assert!(fs::read_link(&link).is_ok(), "staged as a symlink");
        }
        // Second call is a no-op either way (idempotent).
        assert!(!ensure_flannel_alias(&runtime).unwrap());
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
