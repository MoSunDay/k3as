//! Process, identity, namespace and stdio syscalls (T4.11, Q48).
//!
//! Adapted from kiri @ 7846fe0 (Apache-2.0). The caller of [`clone3`] /
//! [`clone_legacy`] must be single-threaded (`clone3(child_stack = 0)` is
//! fork-like); `spawn` enforces that before calling.

use std::ffi::CStr;
use std::io;
use std::os::fd::{FromRawFd, OwnedFd, RawFd};
use std::path::Path;

use super::{check, cstr, CloneArgs};

pub fn sethostname(name: &[u8]) -> io::Result<()> {
    // SAFETY: `name` is a valid byte slice; the kernel only reads it.
    check(
        unsafe { libc::sethostname(name.as_ptr().cast(), name.len()) } as libc::c_long,
        "sethostname",
    )
    .map(drop)
}

#[allow(dead_code)] // used by the P2 exec-into-container path
pub fn setdomainname(name: &[u8]) -> io::Result<()> {
    // SAFETY: `name` is a valid byte slice; the kernel only reads it.
    check(
        unsafe { libc::setdomainname(name.as_ptr().cast(), name.len()) } as libc::c_long,
        "setdomainname",
    )
    .map(drop)
}

#[allow(dead_code)] // used by the P2 exec/netns join path
pub fn setns(fd: RawFd, nstype: libc::c_int) -> io::Result<()> {
    // SAFETY: `fd` is an open namespace descriptor owned by the caller.
    check(unsafe { libc::setns(fd, nstype) } as libc::c_long, "setns").map(drop)
}

/// Arms the parent-death signal: the kernel takes this process down with
/// `SIGKILL` when its parent dies, so a crashed engine cannot leave a
/// half-built container (or its cgroup) behind. Linux clears the setting on
/// a credential change, so the child re-arms it after `setgid`/`setuid` and
/// immediately before `execve`.
pub fn set_pdeathsig() -> io::Result<()> {
    // SAFETY: prctl(PR_SET_PDEATHSIG, sig) takes integer arguments only; the
    // variadic argument is a valid signal number and no pointer is read.
    check(
        unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) } as libc::c_long,
        "prctl(PR_SET_PDEATHSIG)",
    )
    .map(drop)
}

/// The current parent pid; `1` means init adopted us, i.e. the engine is
/// already gone.
pub fn getppid() -> libc::pid_t {
    // SAFETY: getppid has no arguments, no preconditions and cannot fail.
    unsafe { libc::getppid() }
}

pub fn setsid() {
    // SAFETY: no arguments, no preconditions; failure (already a leader)
    // is harmless for our own session.
    unsafe { libc::setsid() };
}

pub fn dup2(from: RawFd, to: RawFd) -> io::Result<()> {
    // SAFETY: both descriptors are process-local integers; dup2 checks them.
    check(unsafe { libc::dup2(from, to) } as libc::c_long, "dup2").map(drop)
}

pub fn open_dev_null() -> io::Result<RawFd> {
    // SAFETY: valid NUL-terminated path.
    let fd = unsafe { libc::open(c"/dev/null".as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(fd)
}

pub fn pipe2() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0 as RawFd; 2];
    check(
        // SAFETY: `fds` has room for both descriptors; O_CLOEXEC is
        // available on every kernel this engine supports.
        unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } as libc::c_long,
        "pipe2",
    )?;
    // SAFETY: pipe2 succeeded, so both descriptors are fresh and owned here.
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

pub fn set_nonblocking(fd: RawFd) -> io::Result<()> {
    // SAFETY: `fd` is a descriptor the caller owns.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: same descriptor; F_SETFL consumes the int flags.
    check(
        unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } as libc::c_long,
        "fcntl(O_NONBLOCK)",
    )
    .map(drop)
}

pub fn write_all(fd: RawFd, mut bytes: &[u8]) -> io::Result<()> {
    while !bytes.is_empty() {
        // SAFETY: `bytes` is a valid buffer and `fd` is an open descriptor.
        let n = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        bytes = &bytes[n as usize..];
    }
    Ok(())
}

pub fn read(fd: RawFd, buf: &mut [u8]) -> io::Result<usize> {
    loop {
        // SAFETY: `buf` is writable and `fd` is an open descriptor.
        let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        return Ok(n as usize);
    }
}

pub fn kill(pid: libc::pid_t) {
    // SAFETY: plain pid argument; ESRCH means it is already gone.
    unsafe { libc::kill(pid, libc::SIGKILL) };
}

/// Writes a child-side failure straight to the report descriptor and exits
/// without unwinding. Only used between clone and exec.
pub fn child_fail(report: RawFd, what: &str, err: io::Error) -> ! {
    let msg = format!("nsrun: cannot start container: {what}: {err}\n");
    // SAFETY: valid buffer/descriptor; `_exit` never returns.
    unsafe {
        libc::write(report, msg.as_ptr().cast(), msg.len());
        libc::_exit(127)
    }
}

/// Writes a child-side failure straight to fd 2 and exits. Used after the
/// start report, when the report pipe is already closed by the parent.
pub fn child_fail_fd2(what: &str, err: io::Error) -> ! {
    let msg = format!("nsrun: cannot start container: {what}: {err}\n");
    // SAFETY: valid buffer/descriptor; `_exit` never returns.
    unsafe {
        libc::write(2, msg.as_ptr().cast(), msg.len());
        libc::_exit(125)
    }
}

/// Reports a failed `execve` with Docker's exit-code convention.
pub fn exec_fail(report: RawFd, exe: &CStr, err: io::Error) -> ! {
    let code = match err.raw_os_error() {
        Some(libc::EACCES | libc::EISDIR) => 126,
        Some(libc::ENOEXEC) => 255,
        _ => 127,
    };
    let msg = format!("nsrun: exec {}: {err}\n", exe.to_string_lossy());
    // SAFETY: valid buffer/descriptor; `_exit` never returns.
    unsafe {
        libc::write(report, msg.as_ptr().cast(), msg.len());
        libc::_exit(code)
    }
}

pub fn execve(exe: &CStr, argv: &[*const libc::c_char], envp: &[*const libc::c_char]) -> io::Error {
    // SAFETY: both arrays are NULL-terminated and outlive the call.
    unsafe { libc::execve(exe.as_ptr(), argv.as_ptr(), envp.as_ptr()) };
    io::Error::last_os_error()
}

pub fn waitpid_nohang(pid: libc::pid_t) -> io::Result<Option<i32>> {
    let mut status = 0;
    loop {
        // SAFETY: `status` is writable; WNOHANG never blocks.
        let ret = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        if ret < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        if ret == 0 {
            return Ok(None);
        }
        return Ok(Some(decode_status(status)));
    }
}

pub fn waitpid_blocking(pid: libc::pid_t) -> io::Result<i32> {
    let mut status = 0;
    loop {
        // SAFETY: `status` is writable; the child is ours to reap.
        let ret = unsafe { libc::waitpid(pid, &mut status, 0) };
        if ret == pid {
            return Ok(decode_status(status));
        }
        if ret < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
    }
}

fn decode_status(status: libc::c_int) -> i32 {
    if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status)
    } else if libc::WIFSIGNALED(status) {
        128 + libc::WTERMSIG(status)
    } else {
        255
    }
}

pub fn drop_bounding_cap(cap: u64) {
    // SAFETY: PR_CAPBSET_DROP takes a capability number; EINVAL for caps
    // this kernel does not know is deliberately ignored.
    unsafe {
        libc::prctl(libc::PR_CAPBSET_DROP, cap as libc::c_ulong, 0, 0, 0);
    }
}

pub fn set_no_new_privs() -> io::Result<()> {
    // SAFETY: a plain prctl with no pointer arguments.
    check(
        unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } as libc::c_long,
        "PR_SET_NO_NEW_PRIVS",
    )
    .map(drop)
}

pub fn setgroups_empty() -> io::Result<()> {
    // SAFETY: a zero-length group list is always valid.
    check(
        unsafe { libc::setgroups(0, std::ptr::null()) } as libc::c_long,
        "setgroups",
    )
    .map(drop)
}

pub fn setgid(gid: libc::gid_t) -> io::Result<()> {
    // SAFETY: plain id argument.
    check(unsafe { libc::setgid(gid) } as libc::c_long, "setgid").map(drop)
}

pub fn setuid(uid: libc::uid_t) -> io::Result<()> {
    // SAFETY: plain id argument.
    check(unsafe { libc::setuid(uid) } as libc::c_long, "setuid").map(drop)
}

/// `struct ifreq` is 40 bytes on 64-bit Linux; only the flags view is used.
#[repr(C)]
struct IfReqFlags {
    name: [libc::c_char; libc::IFNAMSIZ],
    flags: libc::c_short,
    _pad: [u8; 22],
}

/// Brings `lo` up in a fresh network namespace.
pub fn loopback_up() -> io::Result<()> {
    // SAFETY: plain socket for the ioctl.
    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut req = IfReqFlags {
        name: [0; libc::IFNAMSIZ],
        flags: 0,
        _pad: [0; 22],
    };
    for (dst, src) in req.name.iter_mut().zip(b"lo\0".iter()) {
        *dst = *src as libc::c_char;
    }
    // SAFETY: `req` is a valid ifreq-sized buffer for both ioctls.
    let get = unsafe { libc::ioctl(fd, libc::SIOCGIFFLAGS, &mut req) };
    if get < 0 {
        let e = io::Error::last_os_error();
        // SAFETY: closing a socket we own.
        unsafe { libc::close(fd) };
        return Err(e);
    }
    req.flags |= (libc::IFF_UP | libc::IFF_RUNNING) as libc::c_short;
    // SAFETY: same valid buffer, now with the requested flags.
    let set = unsafe { libc::ioctl(fd, libc::SIOCSIFFLAGS, &req) };
    let err = (set < 0).then(io::Error::last_os_error);
    // SAFETY: closing a socket we own.
    unsafe { libc::close(fd) };
    match err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// Opens a cgroup directory for `CLONE_INTO_CGROUP`.
pub fn open_cgroup_dir(path: &Path) -> io::Result<OwnedFd> {
    let c = cstr(path)?;
    // SAFETY: valid NUL-terminated path; the descriptor is owned right away.
    let fd = unsafe {
        libc::open(
            c.as_ptr(),
            libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fresh descriptor nothing else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// `clone3`; the caller must be single-threaded (see `spawn`).
///
/// # Safety
/// As with `fork()`: no other thread may hold a lock the child will need.
pub unsafe fn clone3(args: &CloneArgs) -> io::Result<libc::pid_t> {
    // SAFETY: `args` is a valid clone_args of the stated size; the caller
    // upholds the single-thread requirement.
    let ret = unsafe {
        libc::syscall(
            libc::SYS_clone3,
            args as *const CloneArgs,
            std::mem::size_of::<CloneArgs>(),
        )
    };
    check(ret, "clone3").map(|p| p as libc::pid_t)
}

/// Pre-5.7 fallback: raw `clone(2)` with a zero stack (fork-like).
///
/// # Safety
/// Same single-thread requirement as [`clone3`].
pub unsafe fn clone_legacy(flags: u64) -> io::Result<libc::pid_t> {
    // SAFETY: stack=0 makes this a fork-like clone; the flag word carries
    // SIGCHLD in its low byte exactly as the raw syscall expects.
    let ret = unsafe {
        libc::syscall(
            libc::SYS_clone,
            (flags | libc::SIGCHLD as u64) as libc::c_ulong,
            0usize,
            0usize,
            0usize,
            0usize,
        )
    };
    check(ret, "clone").map(|p| p as libc::pid_t)
}

/// `poll(2)` with an EINTR-safe budget. A signal does not shorten the wait:
/// the retry always receives the time actually left, so callers cannot
/// mistake a handled signal for a timeout. `revents` is what the kernel
/// last wrote, so an expired budget simply leaves it at zero.
pub fn poll(fds: &mut [libc::pollfd], timeout_ms: i32) -> io::Result<()> {
    let deadline = (timeout_ms >= 0)
        .then(|| std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms as u64));
    let mut remaining = timeout_ms;
    loop {
        // SAFETY: `fds` is a valid slice of pollfd; the kernel writes revents.
        let ret = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, remaining) };
        if ret >= 0 {
            return Ok(());
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
        let Some(deadline) = deadline else {
            // Negative timeout: block until something happens; retry as is.
            continue;
        };
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return Ok(());
        }
        remaining = left.as_millis().min(i32::MAX as u128) as i32;
    }
}
