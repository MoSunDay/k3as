//! Safe inputs for the namespace runner (T4.11, Q48).
//!
//! Everything here is plain data: no `libc` type crosses the crate boundary.
//! Validation is pure so it can be tested without root or namespaces.

use std::io;
use std::path::{Path, PathBuf};

/// One bind mount injected before `pivot_root`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindMount {
    pub source: PathBuf,
    /// Absolute path inside the container.
    pub target: PathBuf,
    pub readonly: bool,
}

impl BindMount {
    pub fn new(source: impl Into<PathBuf>, target: impl Into<PathBuf>) -> Self {
        Self {
            source: source.into(),
            target: target.into(),
            readonly: false,
        }
    }

    #[must_use]
    pub fn readonly(mut self) -> Self {
        self.readonly = true;
        self
    }
}

/// cgroup v2 limits applied to the container cgroup before exec (P4 wires
/// these from Pod `resources`; the kernel side lands here in P1).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CgroupLimits {
    /// `memory.max` in bytes.
    pub memory_max: Option<u64>,
    /// `memory.swap.max` in bytes (0 = no swap).
    pub memory_swap_max: Option<u64>,
    /// `cpu.max`, e.g. `"50000 100000"`.
    pub cpu_max: Option<String>,
    /// `pids.max`.
    pub pids_max: Option<u64>,
}

impl CgroupLimits {
    pub fn is_empty(&self) -> bool {
        self.memory_max.is_none()
            && self.memory_swap_max.is_none()
            && self.cpu_max.is_none()
            && self.pids_max.is_none()
    }
}

/// Overlayfs view mounted over the rootfs before it is entered (T4.13
/// M3, Q53): shared unpacked image layers as read-only lowers plus a
/// per-container writable upper. All paths are host-side absolutes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Overlay {
    /// Bottom-up lower dirs (image layer order; the mount helper reverses
    /// them — the kernel wants topmost first).
    pub lower: Vec<PathBuf>,
    pub upper: PathBuf,
    pub work: PathBuf,
}

/// What to run: a rootfs, a command, and the isolation knobs for this slice.
#[derive(Debug, Clone)]
pub struct RunSpec {
    /// Host directory that becomes `/` via `pivot_root`.
    pub rootfs: PathBuf,
    /// `argv` inside the container (argv[0] is resolved inside the rootfs).
    pub argv: Vec<String>,
    /// `KEY=VALUE` entries.
    pub env: Vec<String>,
    /// UTS hostname (the namespace is always fresh).
    pub hostname: Option<String>,
    /// Working directory inside the container.
    pub cwd: PathBuf,
    pub uid: u32,
    pub gid: u32,
    pub mounts: Vec<BindMount>,
    /// Fresh network namespace with loopback up. P1 defaults to the host
    /// network for the smoke run.
    pub new_net: bool,
    /// Join this existing network namespace instead of creating one
    /// (T4.12, Q49): a `/proc/<pid>/ns/net` (or nsfs) path owned by the
    /// pod sandbox engine. Mutually exclusive with `new_net`; loopback
    /// stays under the sandbox's control.
    pub netns: Option<PathBuf>,
    /// Overlayfs lowers/upper mounted over `rootfs` before it is entered
    /// (T4.13 M3, Q53). None keeps `rootfs` a plain directory.
    pub overlay: Option<Overlay>,
    /// Pause mode (T4.12, Q49): after the start gate the child parks
    /// forever instead of exec'ing — the pod-sandbox PID 1 whose only job
    /// is holding the namespaces open. Signals use their default
    /// dispositions, so the parent's waitpid reports `128+WTERMSIG`.
    pub pause: bool,
    pub no_new_privs: bool,
    /// Capability bounding set retained on exec (Docker default by default).
    pub caps: u64,
    /// Basename under `/sys/fs/cgroup/init-pro/` (unique per container).
    pub cgroup_name: Option<String>,
    pub cgroup: CgroupLimits,
}

impl RunSpec {
    pub fn new(rootfs: impl Into<PathBuf>, argv: Vec<String>) -> Self {
        Self {
            rootfs: rootfs.into(),
            argv,
            env: Vec::new(),
            hostname: None,
            cwd: PathBuf::from("/"),
            uid: 0,
            gid: 0,
            mounts: Vec::new(),
            new_net: false,
            netns: None,
            overlay: None,
            pause: false,
            no_new_privs: true,
            caps: crate::caps::DEFAULT_CAPS,
            cgroup_name: None,
            cgroup: CgroupLimits::default(),
        }
    }

    /// Reject inputs that cannot be made safe before touching the kernel.
    pub fn validate(&self) -> io::Result<()> {
        if self.argv.is_empty() || self.argv[0].is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "no command to run",
            ));
        }
        if !self.rootfs.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("rootfs must be absolute: {}", self.rootfs.display()),
            ));
        }
        if !self.cwd.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("cwd must be absolute: {}", self.cwd.display()),
            ));
        }
        for m in &self.mounts {
            validate_target(&m.target)?;
            if m.source.as_os_str().is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("mount {} has an empty source", m.target.display()),
                ));
            }
        }
        if self.netns.is_some() && self.new_net {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "netns and new_net are mutually exclusive (Q49)",
            ));
        }
        if let Some(netns) = &self.netns {
            if !netns.is_absolute() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("netns must be absolute: {}", netns.display()),
                ));
            }
        }
        if let Some(ov) = &self.overlay {
            if ov.lower.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "overlay needs at least one lower dir (Q53)",
                ));
            }
            for p in ov.lower.iter().chain([&ov.upper, &ov.work]) {
                if !p.is_absolute() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("overlay dir must be absolute: {}", p.display()),
                    ));
                }
            }
        }
        if self.pause && (self.uid != 0 || self.gid != 0 || !self.env.is_empty()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "pause mode accepts no user/env customization (Q49 sandbox)",
            ));
        }
        if let Some(name) = &self.cgroup_name {
            if name.is_empty()
                || name.len() > 64
                || !name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("invalid cgroup name: {name:?}"),
                ));
            }
        }
        if let Some(h) = &self.hostname {
            if h.len() > 64 || h.bytes().any(|b| b == 0) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "hostname must be <= 64 bytes and NUL-free",
                ));
            }
        }
        Ok(())
    }
}

fn validate_target(target: &Path) -> io::Result<()> {
    if !target.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("mount target must be absolute: {}", target.display()),
        ));
    }
    if target
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("mount target may not contain ..: {}", target.display()),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> RunSpec {
        RunSpec::new("/tmp/rootfs", vec!["/bin/echo".into(), "hi".into()])
    }

    #[test]
    fn accepts_a_normal_spec() {
        assert!(spec().validate().is_ok());
    }

    #[test]
    fn rejects_empty_argv() {
        let mut s = spec();
        s.argv.clear();
        assert!(s.validate().is_err());
    }

    #[test]
    fn rejects_relative_rootfs_and_cwd() {
        let mut s = spec();
        s.rootfs = PathBuf::from("rootfs");
        assert!(s.validate().is_err());
        let mut s = spec();
        s.cwd = PathBuf::from("etc");
        assert!(s.validate().is_err());
    }

    #[test]
    fn rejects_parent_dir_in_mount_target() {
        let mut s = spec();
        s.mounts.push(BindMount::new("/etc/hosts", "/etc/../hosts"));
        assert!(s.validate().is_err());
    }

    #[test]
    fn netns_and_new_net_are_exclusive() {
        let mut s = spec();
        s.new_net = true;
        s.netns = Some(PathBuf::from("/proc/1/ns/net"));
        assert!(s.validate().is_err());
        s.new_net = false;
        assert!(s.validate().is_ok());
        s.netns = Some(PathBuf::from("proc/1/ns/net"));
        assert!(s.validate().is_err());
    }

    #[test]
    fn overlay_defaults_none_and_validates_paths() {
        assert!(spec().overlay.is_none());
        let ov = |lower: Vec<&str>| Overlay {
            lower: lower.into_iter().map(PathBuf::from).collect(),
            upper: PathBuf::from("/views/c/upper"),
            work: PathBuf::from("/views/c/work"),
        };
        let mut s = spec();
        s.overlay = Some(ov(vec!["/layers/l1", "/layers/l2"]));
        assert!(s.validate().is_ok());
        s.overlay = Some(ov(vec!["layers/l1"]));
        assert!(s.validate().is_err());
        s.overlay = Some(ov(vec![]));
        assert!(s.validate().is_err());
    }

    #[test]
    fn pause_mode_rejects_user_customization() {
        let mut s = RunSpec::new("/tmp/rootfs", vec!["/pause".into()]);
        s.pause = true;
        assert!(s.validate().is_ok());
        s.env.push("X=1".into());
        assert!(s.validate().is_err());
    }

    #[test]
    fn readonly_mounts_and_nonroot_uid_validate_independently() {
        // Q55: visibility is the mount table, write is uid/gid + caps — the
        // two axes are orthogonal, so readonly binds must not constrain the
        // credential fields and vice versa.
        let mut s = spec();
        s.uid = 1000;
        s.gid = 1000;
        s.mounts.push(BindMount::new("/data", "/data").readonly());
        assert!(s.validate().is_ok());
        // Readonly mount with root credentials is equally fine (host-rootfs
        // inspection mounts, T4.14).
        let mut root = spec();
        root.mounts
            .push(BindMount::new("/data", "/data").readonly());
        assert!(root.validate().is_ok());
        // Pause sandbox keeps its uid!=0 lock (Q49) regardless of mounts.
        let mut paused = RunSpec::new("/tmp/rootfs", vec!["/pause".into()]);
        paused.pause = true;
        paused
            .mounts
            .push(BindMount::new("/data", "/data").readonly());
        assert!(paused.validate().is_ok());
        paused.uid = 1000;
        assert!(paused.validate().is_err());
    }

    #[test]
    fn rejects_bad_cgroup_names() {
        let mut s = spec();
        s.cgroup_name = Some("../escape".into());
        assert!(s.validate().is_err());
        s.cgroup_name = Some("pod_1-abc".into());
        assert!(s.validate().is_ok());
    }
}
