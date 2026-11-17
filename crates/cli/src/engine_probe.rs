//! Q51 boot probe: does `--container-engine=builtin` actually work on
//! this host? A throwaway `engine serve` on
//! `<data_dir>/run/engine/_probe.sock` must bind AND answer a ping
//! inside a 15s budget. Any failure — preflight, a live engine already
//! occupying the path, a wedged bind — cleans up (kill+wait+unlink) and
//! selects the containerd fallback with a Warning.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use engine_client::EngineClient;

/// Total budget for spawn + bind + ping (Q51).
const BUDGET: Duration = Duration::from_secs(15);
/// How long a Shutdown'd probe engine may take to exit before SIGKILL.
const REAP: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Builtin,
    FallbackContainerd,
}

pub fn probe_socket(data_dir: &Path) -> PathBuf {
    data_dir.join("run").join("engine").join("_probe.sock")
}

/// Record this boot's engine selection under `<data_dir>/run/engine/mode`
/// so multicall peers (crictl/ctr/containerd) can answer "built-in engine
/// node" instead of hanging on a containerd socket that will never exist.
/// Best-effort: the probe outcome is authoritative per boot (Q51).
pub fn write_mode(data_dir: &Path, builtin: bool) {
    let dir = data_dir.join("run").join("engine");
    if std::fs::create_dir_all(&dir).is_ok() {
        let mode = if builtin { "builtin" } else { "containerd" };
        let _ = std::fs::write(dir.join("mode"), mode);
    }
}

/// Run the probe. Returns the engine selection for this boot.
pub async fn probe(data_dir: &Path, role: &str) -> Outcome {
    match probe_inner(data_dir).await {
        Ok(()) => Outcome::Builtin,
        Err(reason) => {
            tracing::warn!(
                target: "init-pro", role, %reason,
                "built-in engine boot probe failed; falling back to containerd (Q51)"
            );
            Outcome::FallbackContainerd
        }
    }
}

async fn probe_inner(data_dir: &Path) -> Result<(), String> {
    let socket = probe_socket(data_dir);
    let dir = socket
        .parent()
        .ok_or_else(|| "probe socket has no parent".to_string())?;
    std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    // A leftover live engine on the private probe path is unexpected
    // state (Q51): do not replace or adopt it — fail the probe.
    if socket.exists() && UnixStreamConnect::quick(&socket) {
        let _ = std::fs::remove_file(&socket);
        return Err(format!(
            "a live engine already occupies {}",
            socket.display()
        ));
    }
    let _ = std::fs::remove_file(&socket); // stale file

    let mut child = spawn_probe(&socket)?;
    let deadline = tokio::time::Instant::now() + BUDGET;
    // Bind window: the socket must appear while our child is ALIVE (a
    // dead child with an existing path means someone raced us onto it).
    loop {
        if socket.exists() {
            match child.try_wait() {
                Ok(Some(status)) => {
                    let _ = std::fs::remove_file(&socket);
                    return Err(format!("probe engine exited early: {status}"));
                }
                Ok(None) => break,
                Err(e) => {
                    let _ = child.kill();
                    let _ = std::fs::remove_file(&socket);
                    return Err(format!("probe engine wait: {e}"));
                }
            }
        }
        if tokio::time::Instant::now() >= deadline {
            cleanup(&mut child, &socket);
            return Err("probe engine never bound".to_string());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // Ping window (remaining budget).
    let ping = async {
        let client = EngineClient::connect(&socket).await?;
        client.ping().await
    };
    match tokio::time::timeout_at(deadline, ping).await {
        Ok(Ok(true)) => {}
        Ok(Ok(false)) => {
            cleanup(&mut child, &socket);
            return Err("probe engine answered ping with failure".to_string());
        }
        Ok(Err(e)) => {
            cleanup(&mut child, &socket);
            return Err(format!("probe ping: {e}"));
        }
        Err(_) => {
            cleanup(&mut child, &socket);
            return Err("probe ping timed out".to_string());
        }
    }
    // Clean success: ask the engine to exit, then unlink.
    if let Ok(client) = EngineClient::connect(&socket).await {
        let _ = client.shutdown().await;
    }
    reap(&mut child);
    let _ = std::fs::remove_file(&socket);
    Ok(())
}

fn spawn_probe(socket: &Path) -> Result<Child, String> {
    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    Command::new(exe)
        .arg("engine")
        .arg("serve")
        .arg("--socket")
        .arg(socket)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("spawn probe engine: {e}"))
}

fn cleanup(child: &mut Child, socket: &Path) {
    let _ = child.kill();
    reap(child);
    let _ = std::fs::remove_file(socket);
}

fn reap(child: &mut Child) {
    let deadline = std::time::Instant::now() + REAP;
    loop {
        match child.try_wait() {
            Ok(Some(_)) | Err(_) => return,
            Ok(None) if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}

/// Blocking 300ms connect probe (engine-client's connect is async; this
/// is the one synchronous liveness check the pre-clean needs).
struct UnixStreamConnect;

impl UnixStreamConnect {
    fn quick(socket: &Path) -> bool {
        std::os::unix::net::UnixStream::connect(socket).is_ok()
    }
}
