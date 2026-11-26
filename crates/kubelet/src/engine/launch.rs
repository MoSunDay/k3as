//! Engine process launch + lifecycle helpers (T4.12, Q49/Q51).
//!
//! `init-pro engine serve` is re-exec'd from the running binary; the
//! socket dir is `<data_dir>/run/engine/` and is NEVER named
//! `containerd.sock` (the vendored containerd supervisor owns that
//! unlink surface — `runtime/src/lib.rs`).
//!
//! Parent tie (H6/C14): every spawned engine gets a piped stdin plus
//! `--exit-on-stdin-eof`. The write end lives inside the returned
//! `Child` (`child.stdin`), so the engine can never outlive the handle
//! that owns it: a kubelet crash closes the pipe for every engine at
//! once, and dropping a handle without reaping still retires the engine
//! (`engine/src/server.rs` polls the pipe in its accept loop).

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// How long the socket take to appear after spawn (bind is immediate
/// after the pause child, so this only guards a wedged binary).
const SOCKET_WAIT: Duration = Duration::from_secs(5);
const SOCKET_POLL: Duration = Duration::from_millis(50);
/// Reap poll period (`reap` / `kill_and_reap`).
const REAP_POLL: Duration = Duration::from_millis(20);
/// H5 (C14): `<id>.engine.log` is opened in append mode and attempt ids
/// are deterministic, so re-runs of the same object would grow the file
/// for weeks. Over this size the log is truncated at launch: only the
/// latest attempt's post-mortem is kept.
const ENGINE_LOG_CAP: u64 = 1024 * 1024;
/// The engine-side half of the parent tie (parsed in `engine/src/cli.rs`).
const EXIT_ON_STDIN_EOF: &str = "--exit-on-stdin-eof";

pub(crate) fn engine_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("run").join("engine")
}

/// The command line of one engine, before spawn. Pure over its inputs
/// (`spawn_engine` adds the stdio wiring), so the tie flag is testable
/// without a process.
pub(crate) fn engine_command(socket: &Path, pause: bool, host_net: bool) -> io::Result<Command> {
    let exe = std::env::current_exe()?;
    let mut cmd = Command::new(exe);
    cmd.arg("engine").arg("serve").arg("--socket").arg(socket);
    if pause {
        cmd.arg("--pause");
        // Q52: the builtin pause sandbox stays in the host netns; workers
        // join /proc/<pause_pid>/ns/net regardless (flag is pause-only).
        if host_net {
            cmd.arg("--host-net");
        }
    }
    cmd.arg(EXIT_ON_STDIN_EOF);
    Ok(cmd)
}

/// Re-exec `init-pro engine serve --socket PATH [--pause]`.
/// `host_net` (Q52) is pause-only: the builtin pause sandbox stays in
/// the host netns instead of creating a fresh one. stderr lands next to
/// the socket for post-mortem (`<id>.engine.log`, capped -- H5).
/// stdin is a pipe held by the returned `Child` (parent tie -- H6).
pub(crate) fn spawn_engine(socket: &Path, pause: bool, host_net: bool) -> io::Result<Child> {
    let mut cmd = engine_command(socket, pause, host_net)?;
    let log = engine_stderr_path(socket);
    if let Some(parent) = log.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let stderr = open_engine_log(&log)?;
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr));
    cmd.spawn()
}

/// H5 (C14): `append` under the cap, `truncate` at or over it, so a
/// long-lived deterministic id keeps exactly the latest attempt's log.
fn open_engine_log(path: &Path) -> io::Result<File> {
    let truncate = std::fs::metadata(path)
        .map(|m| truncate_log(Some(m.len())))
        .unwrap_or(false);
    std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .append(!truncate)
        .truncate(truncate)
        .open(path)
}

/// Pure half of [`open_engine_log`]: whether a log of `len` bytes is
/// over the cap (a missing file is reported as `None` => append/create).
pub(crate) fn truncate_log(len: Option<u64>) -> bool {
    len.is_some_and(|n| n >= ENGINE_LOG_CAP)
}

fn engine_stderr_path(socket: &Path) -> PathBuf {
    let mut name = socket
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(".engine.log");
    socket.with_file_name(name)
}

/// Waits for the listener path to exist (the engine binds before its
/// first accept; a ping after this proves liveness).
pub(crate) async fn wait_socket(socket: &Path) -> io::Result<()> {
    let deadline = tokio::time::Instant::now() + SOCKET_WAIT;
    while !socket.exists() {
        if tokio::time::Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("engine never bound {}", socket.display()),
            ));
        }
        tokio::time::sleep(SOCKET_POLL).await;
    }
    Ok(())
}

/// Polls `try_wait` for up to `grace`; `Ok(true)` = exited and reaped,
/// `Ok(false)` = still alive at the deadline.
fn wait_bounded(child: &mut Child, grace: Duration) -> io::Result<bool> {
    let deadline = Instant::now() + grace;
    loop {
        if child.try_wait()?.is_some() {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        std::thread::sleep(REAP_POLL);
    }
}

/// Best-effort reap: bounded wait, then SIGKILL. Returns the exit status
/// when the child was ours to wait on.
pub(crate) fn reap(child: &mut Child, grace: Duration) -> io::Result<()> {
    if wait_bounded(child, grace)? {
        return Ok(());
    }
    let _ = child.kill();
    let _ = child.wait();
    Ok(())
}

/// H6/C14 drop-path reap: SIGKILL first (`Child::kill` only signals on
/// Unix -- it never waits), then a BOUNDED wait (never a blocking `wait()`
/// -- this runs inside a `Drop`, possibly on a runtime worker). `true` =
/// the child is gone and reaped; `false` = left to init (SIGKILL is
/// already delivered; no zombie outlives the kubelet).
pub(crate) fn kill_and_reap(child: &mut Child, grace: Duration) -> bool {
    let _ = child.kill();
    wait_bounded(child, grace).unwrap_or(false)
}

#[cfg(test)]
#[path = "launch_tests.rs"]
mod tests;
