//! The cloned child (T4.11, Q48): PID 1 of the container.
//!
//! Adapted from kiri `crates/kiri-init/src/linux/container.rs` @ 7846fe0
//! (Apache-2.0), child half only. Sequence: make mounts private, enter the
//! rootfs, mount proc/sys/dev, apply user mounts, `pivot_root`, hostname,
//! stdio, report ready, wait for the parent's start gate, caps, uid/gid,
//! execve. Never returns.

use std::ffi::CString;
use std::os::fd::AsFd;
use std::path::Path;

use crate::spec::RunSpec;
use crate::{caps, inroot, mount, sys};

#[derive(Clone, Copy)]
pub struct ChildIo {
    pub stdout: libc::c_int,
    pub stderr: libc::c_int,
    pub report: libc::c_int,
    pub gate: libc::c_int,
}

pub fn run_child(spec: &RunSpec, io: ChildIo) -> ! {
    // Before the start report the parent is still reading the report pipe.
    let fail = |what: &str, e: std::io::Error| -> ! { sys::child_fail(io.report, what, e) };
    // After it, the parent has closed the report pipe: talk to fd 2 so the
    // engine forwards a real message (and 125 = engine error) instead of a
    // bare EPIPE / 127.
    let fail2 = |what: &str, e: std::io::Error| -> ! { sys::child_fail_fd2(what, e) };

    // Arm the parent-death signal before building anything: if the engine
    // dies while we are still half-set-up, the kernel must take us down with
    // it instead of leaving an orphan in fresh namespaces. Inside the new
    // PID namespace the engine has no pid here (getppid reads 0), so the
    // probe below only catches an engine that was already gone before the
    // prctl; a death after it is covered by the signal itself.
    sys::set_pdeathsig().unwrap_or_else(|e| fail("parent-death signal", e));
    if sys::getppid() == 1 {
        fail(
            "parent",
            std::io::Error::other(
                "the engine exited before the container armed its parent-death signal",
            ),
        );
    }

    // Mount events must not leak back into the engine's namespace.
    sys::remount(c"/", (libc::MS_REC | libc::MS_PRIVATE) as libc::c_ulong)
        .unwrap_or_else(|e| fail("make mounts private", e));

    let rootfs = sys::cstr(&spec.rootfs).unwrap_or_else(|e| fail("enter rootfs", e));
    mount::enter_rootfs(&rootfs).unwrap_or_else(|e| fail("enter rootfs", e));
    let root = inroot::open_root(Path::new(".")).unwrap_or_else(|e| fail("enter rootfs", e));
    mount::container_filesystems().unwrap_or_else(|e| fail("mount", e));
    for m in &spec.mounts {
        mount::apply_bind(root.as_fd(), m)
            .unwrap_or_else(|e| fail(&format!("mount {}", m.target.display()), e));
    }
    drop(root);
    mount::switch_root().unwrap_or_else(|e| fail("pivot_root", e));

    if let Some(h) = &spec.hostname {
        sys::sethostname(h.as_bytes()).unwrap_or_else(|e| fail("hostname", e));
    }
    if spec.new_net {
        sys::loopback_up().unwrap_or_else(|e| fail("loopback", e));
    }

    // A session of its own, as runc gives every container. Otherwise a
    // terminal's Ctrl-C (or a killpg by the caller) reaches the container.
    // Stdio is wired before the start report: every failure from here on
    // has nowhere to report to but fd 2.
    sys::setsid();
    let null = sys::open_dev_null().unwrap_or_else(|e| fail("stdio", e));
    sys::dup2(null, 0).unwrap_or_else(|e| fail("stdio", e));
    sys::dup2(io.stdout, 1).unwrap_or_else(|e| fail("stdio", e));
    sys::dup2(io.stderr, 2).unwrap_or_else(|e| fail("stdio", e));

    // Setup complete: the parent may still need to move us into a cgroup
    // (CLONE_INTO_CGROUP fallback) before anything is exec'd.
    sys::write_all(io.report, b"S").unwrap_or_else(|e| fail2("start report", e));
    let mut go = [0u8; 1];
    match sys::read(io.gate, &mut go) {
        Ok(1) => {}
        Ok(_) => fail2(
            "start gate",
            std::io::Error::other("the engine closed the start gate"),
        ),
        Err(e) => fail2("start gate", e),
    }

    caps::apply(spec.caps);
    if spec.no_new_privs {
        sys::set_no_new_privs().unwrap_or_else(|e| fail2("no-new-privileges", e));
    }
    // Empty supplementary groups, then drop gid before uid (both are
    // irreversible once uid leaves root).
    if spec.uid != 0 || spec.gid != 0 {
        sys::setgroups_empty().unwrap_or_else(|e| fail2("groups", e));
    }
    sys::setgid(spec.gid).unwrap_or_else(|e| fail2("switch user", e));
    sys::setuid(spec.uid).unwrap_or_else(|e| fail2("switch user", e));

    let cwd = sys::cstr(&spec.cwd).unwrap_or_else(|e| fail2("cwd", e));
    sys::chdir(&cwd).unwrap_or_else(|e| fail2("cwd", e));

    // Everything the child needs is built while it is still alive; pointers
    // stay valid until execve.
    let argv_c: Vec<CString> = spec
        .argv
        .iter()
        .map(|a| CString::new(a.as_bytes()))
        .collect::<Result<_, _>>()
        .unwrap_or_else(|_| fail2("argv", std::io::Error::other("argument contains NUL")));
    let env_c: Vec<CString> = spec
        .env
        .iter()
        .map(|a| CString::new(a.as_bytes()))
        .collect::<Result<_, _>>()
        .unwrap_or_else(|_| fail2("env", std::io::Error::other("environment contains NUL")));
    let exe = argv_c
        .first()
        .cloned()
        .unwrap_or_else(|| fail2("argv", std::io::Error::other("no command")));
    let mut argv: Vec<*const libc::c_char> = argv_c.iter().map(|a| a.as_ptr()).collect();
    argv.push(std::ptr::null());
    let mut envp: Vec<*const libc::c_char> = env_c.iter().map(|a| a.as_ptr()).collect();
    envp.push(std::ptr::null());

    let err = sys::execve(&exe, &argv, &envp);
    sys::exec_fail(2, &exe, err)
}

/// argv[0] of the child after `execve` needs no path rewrite: the kernel
/// resolves it inside the new root. Kept as a function so the parser and
/// child agree on the "no shell" rule.
#[allow(dead_code)] // the P2 image-entrypoint parser reuses this rule
pub fn executable_is_absolute(argv0: &str) -> bool {
    Path::new(argv0).is_absolute()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn argv0_is_not_rewritten() {
        assert!(executable_is_absolute("/bin/echo"));
        assert!(!executable_is_absolute("echo"));
    }
}
