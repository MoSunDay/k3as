//! The single-threaded engine server (T4.11/T4.12, Q48/Q49): one
//! connection at a time, non-blocking frame polls, no thread per
//! container. A pause engine spawns its sandbox child before the accept
//! loop, so a probe always finds a pid to answer with.
//!
//! Shutdown is a poll, not a thread. SIGTERM/SIGINT/SIGHUP only set
//! [`SHUTDOWN`] (see `nsrun::install_shutdown_flag`); the accept loop and
//! the container event slice watch the flag, so a signalled engine reaps its
//! container and removes its cgroup instead of orphaning both.
//!
//! Parent tie (H6/C14): with `--exit-on-stdin-eof` the kubelet that spawned
//! the engine keeps the write end of its stdin pipe inside the `Child`
//! handle (`kubelet/src/engine/launch.rs`). EOF on that pipe = the kubelet
//! died or dropped the handle, and the accept loop exits like it does for a
//! shutdown signal (the `nsrun::Child` drop reaps the pause sandbox). A
//! kubelet dying mid-session is already covered by the peer EOF path in
//! `session.rs` (Q49); the session then returns to this loop -- at once
//! for an idle or running container, at the end of its own bounded wait
//! (the Signal grace) for a pause sandbox ignoring SIGTERM -- and the tie
//! check fires. Only opted-in engines are tied: a hand-started
//! `engine serve` with a closed or `/dev/null` stdin keeps serving.

use std::fs::File;
use std::io::{self, Read};
use std::os::fd::AsFd;
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
    /// Q52: keep the pause sandbox in the engine's (host) netns — no
    /// CLONE_NEWNET, no loopback bring-up; the default stays a fresh netns.
    pub host_net: bool,
    pub netns: Option<String>,
    pub log_path: Option<String>,
    pub mounts: Vec<MountSpec>,
    /// H6/C14: exit when the stdin pipe (the kubelet parent tie) hits EOF.
    pub exit_on_stdin_eof: bool,
}

/// The kubelet parent tie (H6/C14): a non-blocking `read(2)` view of the
/// inherited stdin pipe. The engine closure carries no libc/nix/poll (Q48
/// single-thread + dependency gates), so the descriptor is handled through
/// two std carriers only:
///
/// * `O_NONBLOCK` has no std API for pipes, but it is a flag of the open
///   file DESCRIPTION shared by fd 0 and every dup of it -- and
///   `UnixStream::set_nonblocking` is a plain `fcntl(F_SETFL)` on whatever
///   descriptor it wraps. A throw-away socket VIEW of a dup sets the flag
///   for the description; nothing socket-specific is ever called on it.
/// * the reads go through `File` (`read(2)`, descriptor-type agnostic).
///   A `UnixStream` view would `recv(2)`, which answers `ENOTSOCK` on a
///   pipe -- indistinguishable from EOF in the error arm below.
struct StdinTie {
    stdin: File,
}

impl StdinTie {
    fn open() -> io::Result<Self> {
        let flag_carrier = io::stdin().as_fd().try_clone_to_owned()?;
        UnixStream::from(flag_carrier).set_nonblocking(true)?;
        let stdin = File::from(io::stdin().as_fd().try_clone_to_owned()?);
        Ok(Self { stdin })
    }

    /// `true` once every writer of the pipe is gone (EOF). Stray bytes are
    /// drained and discarded (Q50: stdin frames are refused anyway); a
    /// descriptor error means the tie is unusable, which fails closed.
    fn closed(&mut self) -> bool {
        let mut scratch = [0u8; 256];
        loop {
            match self.stdin.read(&mut scratch) {
                Ok(0) => return true,
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return false,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => return true,
            }
        }
    }
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
    // H6/C14: establish the parent tie BEFORE anything is spawned; an
    // unusable tie (closed stdin) must not leave an orphanable engine.
    let mut tie = if opts.exit_on_stdin_eof {
        match StdinTie::open() {
            Ok(t) => Some(t),
            Err(e) => {
                eprintln!("engine: stdin parent tie unusable: {e}; refusing to serve orphaned");
                return 1;
            }
        }
    } else {
        None
    };
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
        // H6/C14: the kubelet parent tie -- EOF on the stdin pipe means the
        // spawner is gone. Exit like a shutdown (the pause child is reaped
        // by its drop) and unlink the socket so no sweep mistakes the
        // stale file for a live engine.
        if tie.as_mut().is_some_and(StdinTie::closed) {
            eprintln!("engine: stdin EOF (spawner gone); exiting");
            let _ = std::fs::remove_file(socket);
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
/// holding a fresh netns (or joining `netns`, or staying in the host
/// netns with `--host-net`, Q52) open with PID 1 parked. The rootfs is
/// an empty dir beside the socket — pause never execs.
fn spawn_pause(socket: &Path, opts: &ServeOptions) -> io::Result<Child> {
    let rootfs = PathBuf::from(format!("{}.pause-rootfs", socket.display()));
    std::fs::create_dir_all(&rootfs)?;
    let mut run = nsrun::RunSpec::new(rootfs, vec!["/pause".into()]);
    run.pause = true;
    run.netns = opts.netns.as_deref().map(std::path::PathBuf::from);
    // Q52: --host-net keeps the sandbox in the host netns — no unshare,
    // no setns, no lo bring-up. nsrun only adds CLONE_NEWNET when
    // new_net && netns.is_none(), so this stays a zero-config path.
    run.new_net = opts.netns.is_none() && !opts.host_net;
    nsrun::spawn(&run)
}
