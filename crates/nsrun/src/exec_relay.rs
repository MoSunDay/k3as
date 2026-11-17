//! Child-side relay machinery for exec-into-container (T4.12, Q50):
//! `RelayIo` and the two fork halves. Everything here runs between
//! `fork` and `_exit` and never returns; the parent-side orchestration
//! (pipes, gating, the `ExecProc` handle) lives in [`crate::execin`].

use std::ffi::CString;
use std::io;
use std::os::fd::{OwnedFd, RawFd};
use std::path::Path;

use crate::caps;
use crate::execin::candidates;
use crate::{sys, ExecSpec};

/// Raw descriptors the relays work with after fork. The parent's
/// `OwnedFd` guards never run in a relay (it `_exit`s), hence the
/// by-hand `sys::close_fd`.
#[derive(Clone, Copy)]
pub(crate) struct RelayIo {
    pub(crate) stdout_w: RawFd,
    pub(crate) stderr_w: RawFd,
    pub(crate) pid_w: RawFd,
    pub(crate) status_w: RawFd,
    pub(crate) root: RawFd,
    pub(crate) devnull: RawFd,
    /// mnt, ipc, uts, net, cgroup (-1 when the kernel has none), pid.
    pub(crate) ns: [RawFd; 6],
    /// Parent-side ends relay A must close.
    pub(crate) reads: [RawFd; 4],
}

/// Relay A: joins the target namespaces in the `child.rs` order (mnt, ipc,
/// uts, net, cgroup, pid LAST), enters the rootfs with fchdir+chroot("."),
/// then forks relay B and exits with B's decoded status. Never returns.
pub(crate) fn relay_a(spec: &ExecSpec, io: RelayIo) -> ! {
    let fail = |what: &str, e: io::Error| -> ! { sys::child_fail(io.status_w, what, e) };
    for fd in io.reads {
        sys::close_fd(fd);
    }
    sys::set_pdeathsig().unwrap_or_else(|e| fail("parent-death signal", e));
    let join = |fd: RawFd, what: &str, nstype: libc::c_int| {
        sys::setns(fd, nstype).unwrap_or_else(|e| fail(what, e))
    };
    join(
        io.ns[0],
        "join mount namespace",
        libc::CLONE_NEWNS as libc::c_int,
    );
    join(
        io.ns[1],
        "join ipc namespace",
        libc::CLONE_NEWIPC as libc::c_int,
    );
    join(
        io.ns[2],
        "join uts namespace",
        libc::CLONE_NEWUTS as libc::c_int,
    );
    join(
        io.ns[3],
        "join net namespace",
        libc::CLONE_NEWNET as libc::c_int,
    );
    if io.ns[4] >= 0 {
        join(
            io.ns[4],
            "join cgroup namespace",
            libc::CLONE_NEWCGROUP as libc::c_int,
        );
    }
    // pid LAST: it only takes effect for relay B, the fork below.
    join(
        io.ns[5],
        "join pid namespace",
        libc::CLONE_NEWPID as libc::c_int,
    );
    sys::fchdir(io.root).unwrap_or_else(|e| fail("enter container rootfs", e));
    sys::chroot(c".").unwrap_or_else(|e| fail("chroot into container", e));
    if spec.cwd != Path::new("/") {
        if let Ok(cwd) = sys::cstr(&spec.cwd) {
            if sys::chdir(&cwd).is_err() {
                let _ = sys::chdir(c"/");
            }
        }
    }
    match sys::fork() {
        Ok(0) => relay_b(spec, io),
        Ok(b) => {
            // B inherited the write ends; A closes its copies so the
            // parent's EOF detection works.
            for fd in [io.stdout_w, io.stderr_w, io.pid_w, io.status_w] {
                sys::close_fd(fd);
            }
            // Same decode rule as the engine's waitpid helpers
            // (WIFEXITED -> code, WIFSIGNALED -> 128+WTERMSIG, else 255).
            sys::exit_raw(sys::waitpid_blocking(b).unwrap_or(255));
        }
        Err(e) => fail("fork exec relay", e),
    }
}

/// Relay B: born inside the container pid namespace. Wires stdio, drops
/// caps, reports its container-ns pid, then exec's. Never returns.
pub(crate) fn relay_b(spec: &ExecSpec, io: RelayIo) -> ! {
    let fail = |what: &str, e: io::Error| -> ! { sys::child_fail(io.status_w, what, e) };
    sys::set_pdeathsig().unwrap_or_else(|e| fail("parent-death signal", e));
    sys::dup2(io.devnull, 0).unwrap_or_else(|e| fail("stdio", e));
    sys::dup2(io.stdout_w, 1).unwrap_or_else(|e| fail("stdio", e));
    sys::dup2(io.stderr_w, 2).unwrap_or_else(|e| fail("stdio", e));
    caps::apply(caps::DEFAULT_CAPS);
    sys::set_no_new_privs().unwrap_or_else(|e| fail("no_new_privs", e));
    let cstring = |items: &[String], what: &str| -> Vec<CString> {
        items
            .iter()
            .map(|a| CString::new(a.as_bytes()))
            .collect::<Result<_, _>>()
            .unwrap_or_else(|_| fail(what, io::Error::other(format!("{what} contains NUL"))))
    };
    let argv_c = cstring(&spec.argv, "argv");
    let env_c = cstring(&spec.env, "env");
    // The report lands before execve: after it the fd is CLOEXEC-closed
    // and this pid is the container-ns one (B was born post-setns).
    sys::write_all(io.pid_w, &(sys::getpid() as u32).to_le_bytes())
        .unwrap_or_else(|e| fail("pid report", e));
    let mut argvp: Vec<*const libc::c_char> = argv_c.iter().map(|c| c.as_ptr()).collect();
    argvp.push(std::ptr::null());
    let mut envp: Vec<*const libc::c_char> = env_c.iter().map(|c| c.as_ptr()).collect();
    envp.push(std::ptr::null());
    let ladder = candidates(&spec.argv[0]);
    let last = ladder.len().saturating_sub(1);
    for (i, exe) in ladder.iter().enumerate() {
        let err = sys::execve(exe, &argvp, &envp);
        // ENOENT mid-ladder falls through to the next prefix; everything
        // else (and the final ENOENT) reports with Docker's 126/127
        // convention via `exec_fail`.
        if err.raw_os_error() == Some(libc::ENOENT) && i != last {
            continue;
        }
        sys::exec_fail(io.status_w, exe, err);
    }
    fail("argv", io::Error::other("no executable candidates"))
}

/// Opens `/proc/<pid>/ns/<kind>` (O_CLOEXEC via `sys::open_path`).
pub(crate) fn open_ns(pid: libc::pid_t, ns: &str) -> io::Result<OwnedFd> {
    let path = sys::cstr(format!("/proc/{pid}/ns/{ns}"))?;
    sys::open_path(&path, libc::O_RDONLY)
        .map_err(|e| io::Error::new(e.kind(), format!("open /proc/{pid}/ns/{ns}: {e}")))
}

/// The exec gate: `None` means the report pipe hit EOF — relay B's
/// execve succeeded and CLOEXEC closed the writer. `Some(text)` is the
/// pre-exec failure (via `sys::child_fail` / `sys::exec_fail`).
pub(crate) fn read_exec_report(fd: RawFd, timeout_ms: i32) -> io::Result<Option<String>> {
    if !crate::readable(fd, timeout_ms)? {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "the exec relay did not report",
        ));
    }
    let mut buf = [0u8; 4096];
    let n = sys::read(fd, &mut buf)?;
    if n == 0 {
        return Ok(None);
    }
    let mut out = buf[..n].to_vec();
    while out.len() < 64 * 1024 && crate::readable(fd, 500)? {
        match sys::read(fd, &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(k) => out.extend_from_slice(&buf[..k]),
        }
    }
    Ok(Some(String::from_utf8_lossy(&out).into_owned()))
}
