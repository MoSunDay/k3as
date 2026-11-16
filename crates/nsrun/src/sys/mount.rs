//! Mount and root-related syscalls (T4.11, Q48).
//!
//! Adapted from kiri @ 7846fe0 (Apache-2.0). `mount_setattr(2)` is the
//! descriptor-based hardening path: a bind remount through `/proc/self/fd`
//! answers EBUSY on this kernel.

use std::ffi::CStr;
use std::io;
use std::os::fd::{FromRawFd, OwnedFd, RawFd};

use super::check;

pub fn mount(
    src: &CStr,
    target: &CStr,
    fstype: &CStr,
    flags: libc::c_ulong,
    data: Option<&CStr>,
) -> io::Result<()> {
    // SAFETY: every pointer is a valid NUL-terminated string or null.
    let ret = unsafe {
        libc::mount(
            src.as_ptr(),
            target.as_ptr(),
            fstype.as_ptr(),
            flags,
            data.map_or(std::ptr::null(), |d| d.as_ptr().cast()),
        )
    };
    check(
        ret as libc::c_long,
        &format!("mount {}", target.to_string_lossy()),
    )
    .map(drop)
}

/// Mounts with no source or type, for changing propagation (`MS_PRIVATE`).
pub fn remount(target: &CStr, flags: libc::c_ulong) -> io::Result<()> {
    // SAFETY: `target` is a valid NUL-terminated string; the rest is null.
    let ret = unsafe {
        libc::mount(
            std::ptr::null(),
            target.as_ptr(),
            std::ptr::null(),
            flags,
            std::ptr::null(),
        )
    };
    check(
        ret as libc::c_long,
        &format!("remount {}", target.to_string_lossy()),
    )
    .map(drop)
}

/// Linux `struct mount_attr` (include/uapi/linux/mount.h).
#[repr(C)]
struct MountAttr {
    attr_set: u64,
    attr_clr: u64,
    propagation: u64,
    userns_fd: u64,
}

const MOUNT_ATTR_RDONLY: u64 = 0x0000_0001;
const MOUNT_ATTR_NOSUID: u64 = 0x0000_0002;
const MOUNT_ATTR_NODEV: u64 = 0x0000_0004;
const MOUNT_ATTR_NOEXEC: u64 = 0x0000_0008;

/// Hardens an existing mount (read-only, nosuid, nodev, noexec) through
/// `mount_setattr(2)`. A bind remount via `/proc/self/fd/N` answers EBUSY,
/// so the descriptor form is the only reliable one.
pub fn open_path(path: &CStr, flags: libc::c_int) -> io::Result<OwnedFd> {
    // SAFETY: valid NUL-terminated path; the descriptor is owned right away.
    let fd = unsafe { libc::open(path.as_ptr(), flags | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fresh descriptor nothing else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

pub fn harden_mount(fd: RawFd, recursive: bool) -> io::Result<()> {
    let attr = MountAttr {
        attr_set: MOUNT_ATTR_RDONLY | MOUNT_ATTR_NOSUID | MOUNT_ATTR_NODEV | MOUNT_ATTR_NOEXEC,
        attr_clr: 0,
        propagation: 0,
        userns_fd: 0,
    };
    let flags = libc::AT_EMPTY_PATH | if recursive { libc::AT_RECURSIVE } else { 0 };
    // SAFETY: `attr` is a valid mount_attr for the given size and the
    // empty path is paired with AT_EMPTY_PATH, which uses `fd`.
    let ret = unsafe {
        libc::syscall(
            libc::SYS_mount_setattr,
            fd,
            c"".as_ptr(),
            flags,
            &attr as *const MountAttr,
            std::mem::size_of::<MountAttr>(),
        )
    };
    check(ret, "mount_setattr").map(drop)
}

/// `mount_setattr` exists since Linux 5.12; older kernels must fall back
/// to containerd (Q48 preflight).
pub fn mount_setattr_supported() -> bool {
    // SAFETY: a deliberately invalid descriptor is rejected before the
    // kernel looks at the (null) attribute pointer.
    let ret = unsafe {
        libc::syscall(
            libc::SYS_mount_setattr,
            -1,
            c"".as_ptr(),
            libc::AT_EMPTY_PATH,
            std::ptr::null::<MountAttr>(),
            0usize,
        )
    };
    ret >= 0 || io::Error::last_os_error().raw_os_error() != Some(libc::ENOSYS)
}

pub fn umount_lazy(target: &CStr) -> io::Result<()> {
    // SAFETY: `target` is a valid NUL-terminated string.
    check(
        unsafe { libc::umount2(target.as_ptr(), libc::MNT_DETACH) } as libc::c_long,
        "umount2",
    )
    .map(drop)
}

pub fn pivot_root(new_root: &CStr, put_old: &CStr) -> io::Result<()> {
    // SAFETY: both arguments are valid NUL-terminated strings.
    check(
        unsafe { libc::syscall(libc::SYS_pivot_root, new_root.as_ptr(), put_old.as_ptr()) },
        "pivot_root",
    )
    .map(drop)
}
