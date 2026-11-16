//! Shutdown-signal plumbing for the engine process (T4.11, Q48).
//!
//! The engine is single-threaded and polls; it cannot park a thread on
//! `sigwait` and it must not install an async runtime just to learn that
//! someone asked it to stop. So the handlers do the one async-signal-safe
//! thing there is: store `true` into a caller-owned `AtomicBool`. The poll
//! loops (`engine`'s accept loop and its container event slice) read it.
//!
//! `SA_RESTART` is set, matching Rust's own signal handling: an interrupted
//! `read`/`write` resumes instead of failing with `EINTR`. The socket
//! timeouts in the engine still bound a stalled peer.

use std::io;
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};

/// Where the installed handlers publish "a shutdown signal arrived".
///
/// Null until [`install_shutdown_flag`] runs; set exactly once (before any
/// handler can fire) and never cleared. Callers pass a `&'static` flag, so
/// the pointee outlives every signal.
static SHUTDOWN: AtomicPtr<AtomicBool> = AtomicPtr::new(std::ptr::null_mut());

/// Installs SIGTERM/SIGINT/SIGHUP handlers that only set `flag`.
pub fn install_shutdown_flag(flag: &'static AtomicBool) -> io::Result<()> {
    SHUTDOWN.store((flag as *const AtomicBool).cast_mut(), Ordering::Release);
    // SAFETY: `sigaction` is a plain C struct of integers; all-zero is a
    // valid empty disposition, then the handler slot is a plain `extern "C"
    // fn`, the mask is filled by `sigemptyset`, and `sigaction(2)` only
    // reads the struct (no pointer escapes the call).
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = on_signal as *const () as usize;
        action.sa_flags = libc::SA_RESTART;
        libc::sigemptyset(&mut action.sa_mask);
        for sig in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
            crate::sys::check(
                libc::sigaction(sig, &action, std::ptr::null_mut()) as libc::c_long,
                "sigaction",
            )?;
        }
    }
    Ok(())
}

/// `true` once a shutdown signal has arrived. An acquire load is enough: the
/// flag is a one-way latch, never a data hand-off.
pub fn shutdown_requested(flag: &AtomicBool) -> bool {
    flag.load(Ordering::Acquire)
}

/// The handler. Async-signal-safe by construction: one atomic load, a null
/// check and one atomic store — no allocation, no lock, no reentrancy.
extern "C" fn on_signal(_sig: libc::c_int) {
    let flag = SHUTDOWN.load(Ordering::Acquire);
    if flag.is_null() {
        return;
    }
    // SAFETY: `install_shutdown_flag` only ever stores a pointer to a
    // `&'static AtomicBool` that outlives the process, so the dereference is
    // valid here; the pointer is never cleared or replaced.
    let flag = unsafe { &*flag };
    flag.store(true, Ordering::Release);
}
