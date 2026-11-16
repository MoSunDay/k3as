//! The single-threaded engine server (T4.11, Q48): one connection at a
//! time, blocking reads under a socket timeout, no thread per container.
//!
//! Shutdown is a poll, not a thread. SIGTERM/SIGINT/SIGHUP only set
//! [`SHUTDOWN`] (see `nsrun::install_shutdown_flag`); the accept loop and
//! the container event slice watch the flag, so a signalled engine reaps its
//! container and removes its cgroup instead of orphaning both.

use std::io;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use crate::session::{self, Outcome};

/// Set by the installed signal handlers; read by the two poll loops.
pub(crate) static SHUTDOWN: AtomicBool = AtomicBool::new(false);

/// Idle park of the non-blocking accept loop; also the shutdown latency
/// floor while no client is connected.
const ACCEPT_POLL: Duration = Duration::from_millis(20);

pub fn serve(socket: &Path) -> i32 {
    if let Err(e) = nsrun::preflight(false) {
        eprintln!("engine: preflight failed: {e}");
        return 1;
    }
    if let Err(e) = nsrun::ensure_single_threaded() {
        eprintln!("engine: {e}");
        return 1;
    }
    if let Some(parent) = socket.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            eprintln!("engine: create {}: {e}", parent.display());
            return 1;
        }
    }
    // Never unlink a live engine's socket: replacing the path would leave the
    // old engine running but unreachable, still holding its container. A
    // listening engine accepts the probe; a stale file answers ECONNREFUSED.
    if socket.exists() && UnixStream::connect(socket).is_ok() {
        eprintln!(
            "engine: {} is a live engine; refusing to replace it",
            socket.display()
        );
        return 1;
    }
    if let Err(e) = nsrun::install_shutdown_flag(&SHUTDOWN) {
        eprintln!("engine: install signal handlers: {e}");
        return 1;
    }
    let _ = std::fs::remove_file(socket); // stale socket file
    let listener = match UnixListener::bind(socket) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("engine: bind {}: {e}", socket.display());
            return 1;
        }
    };
    if let Err(e) = std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600)) {
        eprintln!("engine: chmod {}: {e}", socket.display());
        return 1;
    }
    if let Err(e) = listener.set_nonblocking(true) {
        eprintln!("engine: nonblocking {}: {e}", socket.display());
        return 1;
    }
    eprintln!("engine: listening on {}", socket.display());
    loop {
        if nsrun::shutdown_requested(&SHUTDOWN) {
            eprintln!("engine: shutdown signal; exiting");
            return 0;
        }
        match listener.accept() {
            Ok((stream, _)) => match session::handle(stream, &listener) {
                Ok(Outcome::Exit) => return 0,
                Ok(Outcome::Continue) => {}
                Err(e) => eprintln!("engine: session ended: {e}"),
            },
            // The only busy-wait in the process, and it is 20 ms.
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => std::thread::sleep(ACCEPT_POLL),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => {
                eprintln!("engine: accept: {e}");
                return 1;
            }
        }
    }
}
