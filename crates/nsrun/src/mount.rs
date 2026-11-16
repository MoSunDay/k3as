//! The child's mount recipe (T4.11, Q48).
//!
//! Adapted from kiri `crates/kiri-init/src/linux/container.rs` @ 7846fe0
//! (Apache-2.0): make the rootfs a mount point, cover `/proc`/`/sys`/`/dev`
//! with fresh filesystems, hide the host-facing proc files, then
//! `pivot_root`. User bind mounts are applied through the secure lookup in
//! [`super::inroot`], never through a plain path walk.

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::os::fd::{AsRawFd, BorrowedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use crate::inroot::{self, Last};
use crate::spec::BindMount;
use crate::sys;

/// Makes `rootfs` a mount point and enters it. Required before
/// `pivot_root`, which refuses a directory that is not a mount point.
pub fn enter_rootfs(rootfs: &std::ffi::CStr) -> io::Result<()> {
    // Mount first, then chdir: a cwd pinned before the bind mount still
    // resolves to the *old* mount, and pivot_root(".", ".") then answers
    // EBUSY (new_root == current root mount).
    sys::mount(
        rootfs,
        rootfs,
        c"none",
        (libc::MS_BIND | libc::MS_REC) as libc::c_ulong,
        None,
    )?;
    sys::chdir(rootfs)
}

pub fn apply_bind(root: BorrowedFd<'_>, m: &BindMount) -> io::Result<()> {
    let meta = fs::metadata(&m.source).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("mount source {}: {e}", m.source.display()),
        )
    })?;
    let dir = meta.is_dir();
    let at = inroot::open(root, &m.target, if dir { Last::Dir } else { Last::File })?;
    let src = sys::cstr(&m.source)?;
    let target = at.proc_path();
    let flags = if dir {
        libc::MS_BIND | libc::MS_REC
    } else {
        libc::MS_BIND
    };
    sys::mount(&src, &target, c"none", flags as libc::c_ulong, None)?;
    if m.readonly {
        // The mount is applied through the secure O_PATH descriptor, but
        // mount_setattr must be addressed by a *path*: reopening
        // /proc/self/fd/N resolves outside the new mount and answers
        // EINVAL. The cwd is the container root, so open the target
        // relative to it (the mount call above already resolved the
        // original path under securejoin).
        let rel = m.target.strip_prefix("/").unwrap_or(&m.target);
        let rel = sys::cstr(rel)?;
        let mounted = sys::open_path(&rel, libc::O_PATH)?;
        sys::harden_mount(mounted.as_raw_fd(), dir)?;
    }
    Ok(())
}

fn ensure_dir(name: &std::ffi::CStr) -> io::Result<()> {
    let path = Path::new(OsStr::from_bytes(name.to_bytes()));
    if fs::symlink_metadata(path)?.is_dir() {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("/{} in the image must be a directory", path.display()),
        ))
    }
}

/// `/proc`, `/sys`, `/dev`, devpts, shm, mqueue and a read-only cgroup view.
/// Relative paths: the current directory is the container root.
pub fn container_filesystems() -> io::Result<()> {
    let nosuid_nodev_noexec = libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC;
    for d in [c"proc", c"sys", c"dev"] {
        sys::mkdir(d, 0o755)?;
        ensure_dir(d)?;
    }
    sys::mount(
        c"proc",
        c"proc",
        c"proc",
        nosuid_nodev_noexec as libc::c_ulong,
        None,
    )?;
    sys::mount(
        c"sysfs",
        c"sys",
        c"sysfs",
        (nosuid_nodev_noexec | libc::MS_RDONLY) as libc::c_ulong,
        None,
    )?;
    // Never devtmpfs: the kernel has one, and remounting it hands the
    // container the host's own /dev.
    sys::mount(
        c"tmpfs",
        c"dev",
        c"tmpfs",
        libc::MS_NOSUID as libc::c_ulong,
        Some(c"mode=755,size=65536k"),
    )?;
    for (name, major, minor) in [
        (c"dev/null", 1, 3),
        (c"dev/zero", 1, 5),
        (c"dev/full", 1, 7),
        (c"dev/random", 1, 8),
        (c"dev/urandom", 1, 9),
        (c"dev/tty", 5, 0),
    ] {
        sys::mknod(name, libc::S_IFCHR | 0o666, major, minor)?;
        // mknod applies the umask; every user needs these.
        sys::chmod(name, 0o666)?;
    }
    for (target, link) in [
        (c"/proc/self/fd", c"dev/fd"),
        (c"/proc/self/fd/0", c"dev/stdin"),
        (c"/proc/self/fd/1", c"dev/stdout"),
        (c"/proc/self/fd/2", c"dev/stderr"),
        (c"pts/ptmx", c"dev/ptmx"),
    ] {
        sys::symlink(target, link)?;
    }
    for d in [c"dev/pts", c"dev/shm", c"dev/mqueue"] {
        sys::mkdir(d, 0o755)?;
    }
    sys::mount(
        c"devpts",
        c"dev/pts",
        c"devpts",
        (libc::MS_NOSUID | libc::MS_NOEXEC) as libc::c_ulong,
        Some(c"newinstance,ptmxmode=0666,mode=0620,gid=5"),
    )?;
    sys::mount(
        c"shm",
        c"dev/shm",
        c"tmpfs",
        nosuid_nodev_noexec as libc::c_ulong,
        Some(c"mode=1777,size=65536k"),
    )?;
    sys::mount(
        c"mqueue",
        c"dev/mqueue",
        c"mqueue",
        nosuid_nodev_noexec as libc::c_ulong,
        None,
    )?;
    sys::mkdir(c"sys/fs/cgroup", 0o755)?;
    sys::mount(
        c"cgroup2",
        c"sys/fs/cgroup",
        c"cgroup2",
        (nosuid_nodev_noexec | libc::MS_RDONLY) as libc::c_ulong,
        None,
    )?;
    harden_proc()?;
    Ok(())
}

/// Docker's default readonly/masked paths: the container is real root (no
/// user namespace), so these non-namespaced proc files must not be writable.
fn harden_proc() -> io::Result<()> {
    let ro = libc::MS_BIND
        | libc::MS_REMOUNT
        | libc::MS_RDONLY
        | libc::MS_NOSUID
        | libc::MS_NODEV
        | libc::MS_NOEXEC;
    // `/proc/sys` holds writable sysctls (core_pattern, modprobe, ...): a
    // path to host code. This one must succeed.
    sys::mount(
        c"proc/sys",
        c"proc/sys",
        c"none",
        libc::MS_BIND as libc::c_ulong,
        None,
    )?;
    sys::remount(c"proc/sys", ro as libc::c_ulong)?;
    // The rest are best-effort: a path the kernel lacks is already absent.
    for p in [c"proc/sysrq-trigger", c"proc/irq", c"proc/bus", c"proc/fs"] {
        if sys::mount(p, p, c"none", libc::MS_BIND as libc::c_ulong, None).is_ok() {
            let _ = sys::remount(p, ro as libc::c_ulong);
        }
    }
    for p in [
        c"proc/kcore",
        c"proc/keys",
        c"proc/latency_stats",
        c"proc/timer_list",
        c"proc/timer_stats",
        c"proc/sched_debug",
        c"proc/interrupts",
    ] {
        let _ = sys::mount(
            c"dev/null",
            p,
            c"none",
            libc::MS_BIND as libc::c_ulong,
            None,
        );
    }
    for p in [
        c"proc/scsi",
        c"proc/acpi",
        c"proc/asound",
        c"sys/firmware",
        c"sys/devices/virtual/powercap",
    ] {
        let _ = sys::mount(
            c"tmpfs",
            p,
            c"tmpfs",
            (libc::MS_RDONLY | libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC) as libc::c_ulong,
            Some(c"size=0k"),
        );
    }
    Ok(())
}

/// `pivot_root(".", ".")` + detach the old root, leaving only the image.
pub fn switch_root() -> io::Result<()> {
    sys::pivot_root(c".", c".")?;
    sys::umount_lazy(c".")?;
    sys::chdir(c"/")
}
