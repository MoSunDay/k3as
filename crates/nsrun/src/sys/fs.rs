//! Path syscalls used while building the container root (T4.11, Q48).
//!
//! Adapted from kiri @ 7846fe0 (Apache-2.0).

use std::ffi::CStr;
use std::io;
use std::os::fd::RawFd;

use super::check;

pub fn mkdir(path: &CStr, mode: libc::mode_t) -> io::Result<()> {
    // SAFETY: valid NUL-terminated path.
    let ret = unsafe { libc::mkdir(path.as_ptr(), mode) };
    if ret == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EEXIST) {
        return Ok(());
    }
    Err(io::Error::last_os_error())
}

pub fn mknod(path: &CStr, mode: libc::mode_t, major: u32, minor: u32) -> io::Result<()> {
    let dev = libc::makedev(major, minor);
    // SAFETY: valid NUL-terminated path; `dev` is a plain device number.
    check(
        unsafe { libc::mknod(path.as_ptr(), mode, dev) } as libc::c_long,
        "mknod",
    )
    .map(drop)
}

pub fn chmod(path: &CStr, mode: libc::mode_t) -> io::Result<()> {
    // SAFETY: valid NUL-terminated path.
    check(
        unsafe { libc::chmod(path.as_ptr(), mode) } as libc::c_long,
        "chmod",
    )
    .map(drop)
}

pub fn symlink(target: &CStr, link: &CStr) -> io::Result<()> {
    // SAFETY: `link` is a valid NUL-terminated string; failures are fine.
    unsafe { libc::unlink(link.as_ptr()) };
    // SAFETY: both arguments are valid NUL-terminated strings.
    check(
        unsafe { libc::symlink(target.as_ptr(), link.as_ptr()) } as libc::c_long,
        "symlink",
    )
    .map(drop)
}

pub fn chdir(path: &CStr) -> io::Result<()> {
    // SAFETY: valid NUL-terminated path.
    check(
        unsafe { libc::chdir(path.as_ptr()) } as libc::c_long,
        "chdir",
    )
    .map(drop)
}

/// Enters the directory behind `fd` (T4.12, Q50 exec): the relay's O_PATH
/// handle on `/proc/<pid>/root` needs no path walk through engine mounts.
pub fn fchdir(fd: RawFd) -> io::Result<()> {
    // SAFETY: `fd` is a descriptor the caller owns; the kernel only reads it.
    check(unsafe { libc::fchdir(fd) } as libc::c_long, "fchdir").map(drop)
}

/// `chroot(2)` on "." after `fchdir(root_fd)`: the nsenter-style way the
/// exec relay enters the container rootfs without pivot_root.
pub fn chroot(path: &CStr) -> io::Result<()> {
    // SAFETY: valid NUL-terminated path.
    check(
        unsafe { libc::chroot(path.as_ptr()) } as libc::c_long,
        "chroot",
    )
    .map(drop)
}
