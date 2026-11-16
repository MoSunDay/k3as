//! Raw Linux syscalls used by the namespace child (T4.11, Q48).
//!
//! Adapted from kiri `crates/kiri-init/src/linux/sys.rs` @ 7846fe0
//! (Apache-2.0), narrowed to the host-side engine: no veth/netlink/DNS.
//!
//! Every call is a thin `io::Result` wrapper; callers never see `libc`
//! types. The crate is the project's only sanctioned `unsafe` island
//! (Q48) and every block carries a SAFETY note.
//!
//! Split by responsibility (file-size rule): [`fs`] path calls, [`mount`]
//! mount/root operations, [`process`] clone/exec/wait/identity/stdio.

use std::ffi::CString;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

/// Highest capability number we know to drop from the bounding set (Linux
/// 5.9+; unknown caps just make `PR_CAPBSET_DROP` answer EINVAL).
pub const LAST_CAP: u64 = 40;

/// Not defined by older libc headers; the kernel ABI value is stable.
pub const CLONE_INTO_CGROUP: u64 = 0x2_0000_0000;

/// Arguments for clone3(2). The layout is `struct clone_args` from
/// `linux/sched.h` (all fields `__aligned_u64`).
#[repr(C)]
#[derive(Default)]
pub struct CloneArgs {
    pub flags: u64,
    pub pidfd: u64,
    pub child_tid: u64,
    pub parent_tid: u64,
    pub exit_signal: u64,
    pub stack: u64,
    pub stack_size: u64,
    pub tls: u64,
    pub set_tid: u64,
    pub set_tid_size: u64,
    pub cgroup: u64,
}

pub fn check(ret: libc::c_long, what: &str) -> io::Result<libc::c_long> {
    if ret < 0 {
        let e = io::Error::last_os_error();
        return Err(io::Error::new(e.kind(), format!("{what}: {e}")));
    }
    Ok(ret)
}

pub fn cstr(p: impl AsRef<Path>) -> io::Result<CString> {
    CString::new(p.as_ref().as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))
}

mod fs;
mod mount;
mod process;

pub use fs::*;
pub use mount::*;
pub use process::*;
