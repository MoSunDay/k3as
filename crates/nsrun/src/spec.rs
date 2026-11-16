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
    /// Fresh network namespace with loopback up. P2 joins the CNI sandbox
    /// netns instead; P1 defaults to the host network for the smoke run.
    pub new_net: bool,
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
    fn rejects_bad_cgroup_names() {
        let mut s = spec();
        s.cgroup_name = Some("../escape".into());
        assert!(s.validate().is_err());
        s.cgroup_name = Some("pod_1-abc".into());
        assert!(s.validate().is_ok());
    }
}
