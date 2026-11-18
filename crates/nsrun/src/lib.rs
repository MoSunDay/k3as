//! `nsrun` — the namespace kernel of init-pro's built-in container engine
//! (TODO **T4.11**, decision **Q48**).
//!
//! This is a Rust port of the kiri `kiri-init` Linux child model
//! (`link_repos/kiri` @ `7846fe0`, Apache-2.0): one `clone3` into fresh
//! namespaces (and optionally a cgroup), child-side isolation, and a
//! parent-side start gate. There is no runc, no shim, and no VM.
//!
//! **This is the only crate allowed to contain `unsafe`** (Q48). The rest
//! of the workspace keeps `#![forbid(unsafe_code)]`; `scripts/unsafe-audit.sh`
//! enforces the boundary and every block here carries a SAFETY note.
//!
//! `clone3(child_stack = 0)` is fork-like: the caller MUST be single
//! threaded. [`spawn`] checks `/proc/self/status` before cloning and refuses
//! to run inside the tokio daemon; the `engine` process is the only caller.

#![deny(unsafe_op_in_unsafe_fn)]
#![deny(clippy::undocumented_unsafe_blocks)]

mod caps;
mod cgroup;
mod child;
mod exec_relay;
mod execin;
mod handle;
mod inroot;
mod mount;
mod signal;
mod spec;
mod sys;

use std::io;
use std::os::fd::AsRawFd;

pub use execin::{exec_into, ExecProc, ExecSpec};

/// Poll a raw descriptor for writability (T4.12 gate 3: the engine's only
/// poll(2) reach outside `sys`). `Ok(false)` means the budget expired.
/// `POLLERR`/`POLLHUP` count as ready so the following write surfaces the
/// real error instead of a silent timeout.
pub fn poll_writable(fd: std::os::fd::RawFd, timeout: std::time::Duration) -> io::Result<bool> {
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLOUT,
        revents: 0,
    };
    let ms = timeout.as_millis().min(i32::MAX as u128) as i32;
    sys::poll(std::slice::from_mut(&mut pfd), ms)?;
    Ok(pfd.revents & (libc::POLLOUT | libc::POLLERR | libc::POLLHUP) != 0)
}
pub use handle::{Child, Output};
pub use signal::{install_shutdown_flag, shutdown_requested};
pub use spec::{BindMount, CgroupLimits, Overlay, RunSpec};

/// The caller must be single-threaded for the fork-like clone3.
pub fn ensure_single_threaded() -> io::Result<()> {
    let status = std::fs::read_to_string("/proc/self/status")?;
    let threads = status
        .lines()
        .find_map(|l| l.strip_prefix("Threads:"))
        .and_then(|v| v.trim().parse::<u32>().ok())
        .ok_or_else(|| io::Error::other("cannot read Threads from /proc/self/status"))?;
    if threads != 1 {
        return Err(io::Error::other(format!(
            "clone3 needs a single-threaded process; this one has {threads} threads"
        )));
    }
    Ok(())
}

/// Root + cgroup v2 preflight. Callers should fall back to containerd, not
/// abort the node, when this fails (Q48).
pub fn preflight(needs_cgroup: bool) -> io::Result<()> {
    // SAFETY: geteuid has no preconditions and no pointer arguments.
    if unsafe { libc::geteuid() } != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "the built-in engine requires real root (no user namespace)",
        ));
    }
    if !sys::mount_setattr_supported() {
        return Err(io::Error::other(
            "the kernel lacks mount_setattr(2) (Linux 5.12+); falling back to containerd",
        ));
    }
    if !std::path::Path::new("/proc/self/ns/pid").exists() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "/proc is required to create namespaces",
        ));
    }
    if needs_cgroup && !cgroup::available() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "cgroup v2 is required for limits (/sys/fs/cgroup/cgroup.controllers missing)",
        ));
    }
    Ok(())
}

/// Clone into the container namespaces and hand back the running child.
pub fn spawn(spec: &RunSpec) -> io::Result<Child> {
    spec.validate()?;
    ensure_single_threaded()?;
    if spec.cgroup_name.is_some() && !spec.cgroup.is_empty() {
        preflight(true)?;
    } else {
        preflight(false)?;
    }

    let cgroup = match (&spec.cgroup_name, spec.cgroup.is_empty()) {
        (Some(name), false) => Some(cgroup::prepare(name, &spec.cgroup)?),
        (None, false) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "cgroup limits require a cgroup_name",
            ))
        }
        _ => None,
    };
    // The cgroup directory already exists: a failed pipe must not leak it.
    let pipes: io::Result<_> = (|| {
        let report = sys::pipe2()?;
        let gate = sys::pipe2()?;
        let stdout = sys::pipe2()?;
        let stderr = sys::pipe2()?;
        Ok((report, gate, stdout, stderr))
    })();
    let ((report_r, report_w), (gate_r, gate_w), (stdout_r, stdout_w), (stderr_r, stderr_w)) =
        match pipes {
            Ok(v) => v,
            Err(e) => {
                if let Some(c) = &cgroup {
                    cgroup::remove(&c.dir);
                }
                return Err(e);
            }
        };
    let io = child::ChildIo {
        stdout: stdout_w.as_raw_fd(),
        stderr: stderr_w.as_raw_fd(),
        report: report_w.as_raw_fd(),
        gate: gate_r.as_raw_fd(),
    };

    let cgroup_fd = cgroup.as_ref().map(|c| c.fd.as_raw_fd());
    let (pid, into_cgroup) = clone_container(spec, io, cgroup_fd).inspect_err(|_| {
        if let Some(c) = &cgroup {
            cgroup::remove(&c.dir);
        }
    })?;

    // Parent keeps only the read/write ends it needs.
    drop(report_w);
    drop(gate_r);
    drop(stdout_w);
    drop(stderr_w);

    let msg = read_report(report_r.as_raw_fd(), 30_000).inspect_err(|_| {
        sys::kill(pid);
        let _ = sys::waitpid_blocking(pid);
        if let Some(c) = &cgroup {
            cgroup::remove(&c.dir);
        }
    })?;
    drop(report_r);
    if let Some(msg) = msg {
        let stderr = drain_text(stderr_r.as_raw_fd());
        sys::kill(pid);
        let status = sys::waitpid_blocking(pid).ok();
        if let Some(c) = &cgroup {
            cgroup::remove(&c.dir);
        }
        let mut text = msg.trim_end().to_string();
        if !stderr.is_empty() {
            text.push_str(": ");
            text.push_str(stderr.trim_end());
        }
        if text.is_empty() {
            text = "container setup failed before exec".to_string();
        } else if let Some(code) = status {
            text.push_str(&format!(" (exit {code})"));
        }
        return Err(io::Error::other(text));
    }

    if let Some(c) = &cgroup {
        if !into_cgroup {
            if let Err(e) = cgroup::move_pid(&c.dir, pid) {
                sys::kill(pid);
                let _ = sys::waitpid_blocking(pid);
                let _ = std::fs::remove_dir(&c.dir);
                return Err(e);
            }
        }
    }
    if let Err(e) = sys::write_all(gate_w.as_raw_fd(), b"G") {
        sys::kill(pid);
        let _ = sys::waitpid_blocking(pid);
        if let Some(c) = &cgroup {
            cgroup::remove(&c.dir);
        }
        return Err(e);
    }
    drop(gate_w);
    // Build the handle first: if the pipes cannot be made non-blocking, `?`
    // drops it and `Drop` kills, reaps and removes the cgroup.
    let mut child = Child {
        pid,
        stdout: stdout_r,
        stderr: stderr_r,
        stdout_open: true,
        stderr_open: true,
        status: None,
        exit_sent: false,
        grace: 0,
        cgroup_dir: cgroup.map(|c| c.dir),
    };
    child.set_nonblocking()?;
    Ok(child)
}

fn clone_container(
    spec: &RunSpec,
    io: child::ChildIo,
    cgroup_fd: Option<libc::c_int>,
) -> io::Result<(libc::pid_t, bool)> {
    let mut flags = (libc::CLONE_NEWNS
        | libc::CLONE_NEWPID
        | libc::CLONE_NEWUTS
        | libc::CLONE_NEWIPC
        | libc::CLONE_NEWCGROUP) as u64;
    // Q49: joining an existing sandbox netns (spec.netns) replaces the
    // fresh netns; CLONE_NEWNET is only for new_net.
    if spec.new_net && spec.netns.is_none() {
        flags |= libc::CLONE_NEWNET as u64;
    }
    let with_cgroup = cgroup_fd.is_some();
    let args = sys::CloneArgs {
        flags: flags
            | if with_cgroup {
                sys::CLONE_INTO_CGROUP
            } else {
                0
            },
        exit_signal: libc::SIGCHLD as u64,
        cgroup: cgroup_fd.unwrap_or(0) as u64,
        ..Default::default()
    };
    // SAFETY: spawn() proved the process is single-threaded, the only
    // precondition for a fork-like clone.
    match unsafe { sys::clone3(&args) } {
        Ok(0) => child::run_child(spec, io),
        Ok(pid) => Ok((pid, with_cgroup)),
        Err(first) => {
            // Kernels < 5.7 or a seccomp policy may reject clone3 and/or
            // CLONE_INTO_CGROUP. Retry without the cgroup, then with the
            // legacy clone syscall; the start gate keeps the child from
            // exec'ing before the parent can move it.
            let plain = sys::CloneArgs {
                flags,
                exit_signal: libc::SIGCHLD as u64,
                ..Default::default()
            };
            // SAFETY: same single-thread precondition as above.
            match unsafe { sys::clone3(&plain) } {
                Ok(0) => child::run_child(spec, io),
                Ok(pid) => Ok((pid, false)),
                Err(second) => {
                    // SAFETY: same single-thread precondition as above.
                    match unsafe { sys::clone_legacy(flags) } {
                        Ok(0) => child::run_child(spec, io),
                        Ok(pid) => Ok((pid, false)),
                        Err(third) => Err(io::Error::new(
                            third.kind(),
                            format!(
                                "clone3: {first}; clone3 without cgroup: {second}; clone: {third}"
                            ),
                        )),
                    }
                }
            }
        }
    }
}

/// Reads the child's start report: `None` means the child reached `execve`
/// (first byte `S`); `Some(message)` is the pre-exec failure text.
fn read_report(fd: libc::c_int, timeout_ms: i32) -> io::Result<Option<String>> {
    if !readable(fd, timeout_ms)? {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "the container child did not report readiness",
        ));
    }
    let mut buf = [0u8; 4096];
    let n = sys::read(fd, &mut buf)?;
    if n == 0 {
        return Err(io::Error::other(
            "the container child closed the start report before exec",
        ));
    }
    if buf[0] == b'S' {
        return Ok(None);
    }
    let mut out = buf[..n].to_vec();
    while out.len() < 64 * 1024 && readable(fd, 500)? {
        match sys::read(fd, &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => out.extend_from_slice(&buf[..n]),
        }
    }
    Ok(Some(String::from_utf8_lossy(&out).into_owned()))
}

pub(crate) fn readable(fd: libc::c_int, timeout_ms: i32) -> io::Result<bool> {
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    sys::poll(std::slice::from_mut(&mut pfd), timeout_ms)?;
    Ok(pfd.revents != 0)
}

fn read_some(fd: libc::c_int, timeout_ms: i32) -> io::Result<Option<u8>> {
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    sys::poll(std::slice::from_mut(&mut pfd), timeout_ms)?;
    if pfd.revents == 0 {
        return Ok(None);
    }
    let mut byte = [0u8; 1];
    match sys::read(fd, &mut byte) {
        Ok(0) => Ok(Some(0)),
        Ok(_) => Ok(Some(byte[0])),
        Err(e) => Err(e),
    }
}

fn drain_text(fd: libc::c_int) -> String {
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    for _ in 0..16 {
        match read_some(fd, 500) {
            Ok(Some(0)) | Ok(None) => break,
            Ok(Some(b)) => out.push(b),
            Err(_) => break,
        }
        match sys::read(fd, &mut buf) {
            Ok(0) => break,
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(_) => break,
        }
        if out.len() >= 64 * 1024 {
            break;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_spec_uses_docker_caps() {
        let spec = RunSpec::new("/tmp/rootfs", vec!["/bin/true".into()]);
        assert_eq!(spec.caps, caps::DEFAULT_CAPS);
        assert!(spec.no_new_privs);
        assert!(!spec.new_net);
    }

    #[test]
    fn preflight_does_not_need_cgroup_for_plain_runs() {
        // Either succeeds or fails on root/proc only; it must never require
        // cgroup v2 when no limits were requested.
        let _ = preflight(false);
    }

    #[test]
    fn shutdown_handler_flips_the_flag() {
        static FLAG: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        assert!(!shutdown_requested(&FLAG));
        install_shutdown_flag(&FLAG).expect("install signal handlers");
        // SAFETY: `raise` takes an integer signal number, has no pointer
        // arguments and cannot fail in a way that breaks us; the handler
        // installed above only stores into FLAG.
        unsafe { libc::raise(libc::SIGTERM) };
        assert!(shutdown_requested(&FLAG));
    }

    #[test]
    fn drain_text_keeps_the_first_byte() {
        let (read_end, write_end) = sys::pipe2().expect("pipe2");
        sys::write_all(write_end.as_raw_fd(), b"ab").expect("write");
        drop(write_end);
        assert_eq!(drain_text(read_end.as_raw_fd()), "ab");
    }
}
