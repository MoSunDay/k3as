//! cgroup v2 plumbing for the built-in engine (T4.11, Q48). All writes are
//! path-scoped under `/sys/fs/cgroup/init-pro/<container-id>`.

use std::fs;
use std::io;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};

use crate::spec::CgroupLimits;
use crate::sys;

pub const CGROUP_ROOT: &str = "/sys/fs/cgroup";
const PARENT: &str = "init-pro";

pub struct Handle {
    pub dir: PathBuf,
    pub fd: OwnedFd,
}

pub fn available() -> bool {
    Path::new(CGROUP_ROOT).join("cgroup.controllers").is_file()
}

/// Creates `<root>/init-pro/<name>`, applies the limits, and opens the
/// directory for `CLONE_INTO_CGROUP`.
pub fn prepare(name: &str, limits: &CgroupLimits) -> io::Result<Handle> {
    if !available() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "cgroup v2 is not mounted at /sys/fs/cgroup",
        ));
    }
    let parent = Path::new(CGROUP_ROOT).join(PARENT);
    fs::create_dir_all(&parent)?;
    enable_controllers(&parent);
    let dir = parent.join(name);
    if dir.exists() {
        // A crashed engine can leave an empty cgroup behind. Only ours.
        let _ = fs::remove_dir(&dir);
    }
    fs::create_dir(&dir)
        .map_err(|e| io::Error::new(e.kind(), format!("create {}: {e}", dir.display())))?;
    if let Err(e) = apply_limits(&dir, limits) {
        let _ = fs::remove_dir(&dir);
        return Err(e);
    }
    // The directory exists only for this container: do not leak it when the
    // `CLONE_INTO_CGROUP` descriptor cannot be opened.
    let fd = sys::open_cgroup_dir(&dir).inspect_err(|_| {
        let _ = fs::remove_dir(&dir);
    })?;
    Ok(Handle { dir, fd })
}

fn enable_controllers(parent: &Path) {
    let Ok(available) = fs::read_to_string(parent.join("cgroup.controllers")) else {
        return;
    };
    let mut enable = String::new();
    for c in ["memory", "cpu", "pids"] {
        if available.split_whitespace().any(|x| x == c) {
            enable.push('+');
            enable.push_str(c);
            enable.push(' ');
        }
    }
    if !enable.is_empty() {
        // EBUSY is expected while children exist; limits still apply to a
        // leaf cgroup when its parent has the controller enabled.
        let _ = fs::write(parent.join("cgroup.subtree_control"), enable.trim_end());
    }
}

fn apply_limits(dir: &Path, limits: &CgroupLimits) -> io::Result<()> {
    if let Some(v) = limits.memory_max {
        write(dir, "memory.max", &v.to_string())?;
    }
    if let Some(v) = limits.memory_swap_max {
        // `memory.swap.max` only exists with swap accounting enabled; that
        // one absence is fine, every other failure is real.
        if let Err(e) = write(dir, "memory.swap.max", &v.to_string()) {
            if e.kind() != io::ErrorKind::NotFound {
                return Err(e);
            }
        }
    }
    if let Some(v) = &limits.cpu_max {
        write(dir, "cpu.max", v)?;
    }
    if let Some(v) = limits.pids_max {
        write(dir, "pids.max", &v.to_string())?;
    }
    Ok(())
}

fn write(dir: &Path, file: &str, value: &str) -> io::Result<()> {
    let path = dir.join(file);
    fs::write(&path, value)
        .map_err(|e| io::Error::new(e.kind(), format!("write {}: {e}", path.display())))
}

/// Fallback for kernels without `CLONE_INTO_CGROUP`: move the child before
/// it is released past the start gate.
pub fn move_pid(dir: &Path, pid: libc::pid_t) -> io::Result<()> {
    write(dir, "cgroup.procs", &pid.to_string())
}

/// Best-effort teardown after the child exited and was reaped.
pub fn remove(dir: &Path) {
    if !dir.starts_with(Path::new(CGROUP_ROOT).join(PARENT)) {
        return;
    }
    // cgroup.kill exists since 5.14; removing an empty cgroup is enough on
    // older kernels because the engine killed the child itself.
    let _ = fs::write(dir.join("cgroup.kill"), "1");
    let _ = fs::remove_dir(dir);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remove_refuses_paths_outside_our_parent() {
        let p = Path::new("/tmp/nsrun-cgroup-not-ours");
        // No filesystem effect: the guard returns before any write.
        remove(p);
        assert!(!p.exists());
    }

    /// Path of a cgroup leaf that does not exist. cgroupfs answers ENOENT
    /// for a control file the kernel did not create; a plain tempdir cannot
    /// model that (fs::write would create the file), so the missing
    /// directory plays the part. No tempfile dependency.
    fn absent_leaf(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nsrun-cg-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn missing_memory_swap_file_is_tolerated() {
        let dir = absent_leaf("swap");
        let limits = CgroupLimits {
            memory_swap_max: Some(0),
            ..Default::default()
        };
        assert!(apply_limits(&dir, &limits).is_ok());
    }

    #[test]
    fn other_missing_limit_files_propagate() {
        let dir = absent_leaf("cpu");
        let limits = CgroupLimits {
            cpu_max: Some("50000 100000".into()),
            ..Default::default()
        };
        let err = apply_limits(&dir, &limits).expect_err("missing cpu.max must fail");
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }
}
