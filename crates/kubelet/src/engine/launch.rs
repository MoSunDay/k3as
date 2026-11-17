//! Engine process launch + lifecycle helpers (T4.12, Q49/Q51).
//!
//! `init-pro engine serve` is re-exec'd from the running binary; the
//! socket dir is `<data_dir>/run/engine/` and is NEVER named
//! `containerd.sock` (the vendored containerd supervisor owns that
//! unlink surface — `runtime/src/lib.rs`).

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

/// How long the socket take to appear after spawn (bind is immediate
/// after the pause child, so this only guards a wedged binary).
const SOCKET_WAIT: Duration = Duration::from_secs(5);
const SOCKET_POLL: Duration = Duration::from_millis(50);

pub(crate) fn engine_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("run").join("engine")
}

/// Re-exec `init-pro engine serve --socket PATH [--pause]`.
/// stderr lands next to the socket for post-mortem (`<id>.engine.log`).
pub(crate) fn spawn_engine(socket: &Path, pause: bool) -> io::Result<Child> {
    let exe = std::env::current_exe()?;
    let mut cmd = Command::new(exe);
    cmd.arg("engine").arg("serve").arg("--socket").arg(socket);
    if pause {
        cmd.arg("--pause");
    }
    let log = engine_stderr_path(socket);
    if let Some(parent) = log.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let stderr = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)?;
    cmd.stdout(Stdio::null()).stderr(Stdio::from(stderr));
    cmd.spawn()
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

/// Best-effort reap: bounded wait, then SIGKILL. Returns the exit status
/// when the child was ours to wait on.
pub(crate) fn reap(child: &mut Child, grace: Duration) -> io::Result<()> {
    let deadline = std::time::Instant::now() + grace;
    loop {
        match child.try_wait()? {
            Some(_) => return Ok(()),
            None if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Ok(());
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}
