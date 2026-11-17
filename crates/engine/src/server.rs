//! The single-threaded engine server (T4.11/T4.12, Q48/Q49): one
//! connection at a time, non-blocking frame polls, no thread per
//! container. A pause engine spawns its sandbox child before the accept
//! loop, so a probe always finds a pid to answer with.
//!
//! Shutdown is a poll, not a thread. SIGTERM/SIGINT/SIGHUP only set
//! [`SHUTDOWN`] (see `nsrun::install_shutdown_flag`); the accept loop and
//! the container event slice watch the flag, so a signalled engine reaps its
//! container and removes its cgroup instead of orphaning both.

use std::io;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use engine_proto::MountSpec;
use nsrun::Child;

use crate::session::{self, Outcome, SessionCtx};

/// Set by the installed signal handlers; read by the two poll loops.
pub(crate) static SHUTDOWN: AtomicBool = AtomicBool::new(false);

/// Idle park of the non-blocking accept loop; also the shutdown latency
/// floor while no client is connected.
const ACCEPT_POLL: Duration = Duration::from_millis(20);

/// Serve-time options (T4.12): `--pause` turns the engine into a pod
/// sandbox (Q49), the rest are Run-level defaults.
#[derive(Default)]
pub struct ServeOptions {
    pub pause: bool,
    pub netns: Option<String>,
    pub log_path: Option<String>,
    pub mounts: Vec<MountSpec>,
}

pub fn serve(socket: &Path, opts: &ServeOptions) -> i32 {
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
    let mut pause_child: Option<Child> = None;
    if opts.pause {
        match spawn_pause(socket, opts) {
            Ok(child) => {
                eprintln!("engine: sandbox pause pid {}", child.pid());
                pause_child = Some(child);
            }
            Err(e) => {
                eprintln!("engine: pause sandbox failed: {e}");
                return 1;
            }
        }
    }
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
    let ctx = SessionCtx {
        netns_default: opts.netns.clone(),
        log_default: opts.log_path.as_deref().map(PathBuf::from),
        mounts_default: opts.mounts.clone(),
    };
    eprintln!("engine: listening on {}", socket.display());
    loop {
        if nsrun::shutdown_requested(&SHUTDOWN) {
            eprintln!("engine: shutdown signal; exiting");
            return 0;
        }
        if let Some(child) = pause_child.as_mut() {
            if let Ok(Some(nsrun::Output::Exited(_))) = child.next_event(Duration::ZERO) {
                eprintln!("engine: sandbox exited; shutting down");
                child.cleanup();
                return 0;
            }
        }
        match listener.accept() {
            Ok((stream, _)) => match session::handle(stream, &listener, &ctx, &mut pause_child) {
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

/// Spawns the Q49 sandbox child: a pause-mode container whose only job is
/// holding a fresh netns (or joining `netns`) open with PID 1 parked.
/// The rootfs is an empty dir beside the socket — pause never execs.
fn spawn_pause(socket: &Path, opts: &ServeOptions) -> io::Result<Child> {
    let rootfs = PathBuf::from(format!("{}.pause-rootfs", socket.display()));
    std::fs::create_dir_all(&rootfs)?;
    let mut run = nsrun::RunSpec::new(rootfs, vec!["/pause".into()]);
    run.pause = true;
    run.netns = opts.netns.as_deref().map(std::path::PathBuf::from);
    run.new_net = opts.netns.is_none();
    nsrun::spawn(&run)
}
