//! Q51 boot probe: does `--container-engine=builtin` actually work on
//! this host? A throwaway `engine serve` on
//! `<data_dir>/run/engine/_probe.sock` must bind AND answer a ping
//! inside a 15s budget. Any failure — preflight, a live engine already
//! occupying the path, a wedged bind — cleans up (kill+wait+unlink) and
//! selects the containerd fallback with a Warning.
//!
//! Every wait after the budget is bounded too (batch D, C1): the
//! post-success connect+shutdown by [`REAP`], the pre-clean liveness check
//! by [`LIVE_TIMEOUT`], and the child's `try_wait` poll loop runs on the
//! blocking pool -- nothing here parks an async worker thread.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use engine_client::EngineClient;

/// Total budget for spawn + bind + ping (Q51).
const BUDGET: Duration = Duration::from_secs(15);
/// How long a Shutdown'd probe engine may take to exit before SIGKILL.
const REAP: Duration = Duration::from_secs(5);
/// Bound of the pre-clean liveness connect (a live engine answers at
/// once; a wedged backlog costs the probe this much and no more).
const LIVE_TIMEOUT: Duration = Duration::from_millis(300);

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
    match probe_inner(data_dir, BUDGET, spawn_probe).await {
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

/// The probe itself, parameterized on its spawn so a test can substitute a
/// child that never listens (C1): `spawn` must return a live child.
async fn probe_inner(
    data_dir: &Path,
    budget: Duration,
    spawn: impl Fn(&Path) -> Result<Child, String>,
) -> Result<(), String> {
    let socket = probe_socket(data_dir);
    let dir = socket
        .parent()
        .ok_or_else(|| "probe socket has no parent".to_string())?;
    std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    // A leftover live engine on the private probe path is unexpected
    // state (Q51): do not replace or adopt it — fail the probe.
    if socket.exists() && live(&socket).await {
        let _ = std::fs::remove_file(&socket);
        return Err(format!(
            "a live engine already occupies {}",
            socket.display()
        ));
    }
    let _ = std::fs::remove_file(&socket); // stale file

    let mut child = spawn(&socket)?;
    let deadline = tokio::time::Instant::now() + budget;
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
                    cleanup(child, &socket).await;
                    return Err(format!("probe engine wait: {e}"));
                }
            }
        }
        if tokio::time::Instant::now() >= deadline {
            cleanup(child, &socket).await;
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
            cleanup(child, &socket).await;
            return Err("probe engine answered ping with failure".to_string());
        }
        Ok(Err(e)) => {
            cleanup(child, &socket).await;
            return Err(format!("probe ping: {e}"));
        }
        Err(_) => {
            cleanup(child, &socket).await;
            return Err("probe ping timed out".to_string());
        }
    }
    // Clean success: ask the engine to exit (bounded by REAP, C1), then
    // reap + unlink.
    let stop = async {
        let client = EngineClient::connect(&socket).await?;
        client.shutdown().await
    };
    let _ = tokio::time::timeout(REAP, stop).await;
    reap(child).await;
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

/// Kill + reap + unlink (Q51); consumes the handle -- nothing is left to ask it.
async fn cleanup(mut child: Child, socket: &Path) {
    let _ = child.kill();
    reap(child).await;
    let _ = std::fs::remove_file(socket);
}

/// Wait for the probe engine to exit on its own, SIGKILL + wait at [`REAP`]
/// (Q51). The 20 ms `try_wait` poll is blocking, so it runs on the blocking
/// pool (C1): never on the async worker that awaits it.
async fn reap(child: Child) {
    let _ = tokio::task::spawn_blocking(move || reap_blocking(child)).await;
}

fn reap_blocking(mut child: Child) {
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

/// [`LIVE_TIMEOUT`]-bounded liveness check for the pre-clean (Q51).
async fn live(socket: &Path) -> bool {
    matches!(
        tokio::time::timeout(LIVE_TIMEOUT, tokio::net::UnixStream::connect(socket)).await,
        Ok(Ok(_))
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    fn tmp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("init-pro-probe-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).expect("scratch dir");
        d
    }

    /// `sleep 30` with null stdio: a child that never listens. Its pid is
    /// published so the test can check the probe's kill + reap.
    fn spawn_sleeper(pid: &Arc<AtomicU32>) -> impl Fn(&Path) -> Result<Child, String> + '_ {
        move |_| {
            let child = Command::new("sleep")
                .arg("30")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .map_err(|e| format!("spawn sleeper: {e}"))?;
            pid.store(child.id(), Ordering::Relaxed);
            Ok(child)
        }
    }

    /// `/proc/<pid>` disappears once the corpse is reaped (not merely killed).
    fn alive(pid: u32) -> bool {
        std::fs::metadata(format!("/proc/{pid}")).is_ok()
    }

    /// Batch D (C1): a child that never binds ends the probe at the budget
    /// (not at its own lifetime), and the probe kills + reaps it on the
    /// blocking pool -- the async worker is never parked in a poll loop.
    #[tokio::test]
    async fn never_listening_child_ends_the_probe_at_the_budget() {
        let pid = Arc::new(AtomicU32::new(0));
        let budget = Duration::from_millis(300);
        let start = std::time::Instant::now();
        let res = probe_inner(&tmp_dir("never"), budget, spawn_sleeper(&pid)).await;
        let elapsed = start.elapsed();
        assert_eq!(res.unwrap_err(), "probe engine never bound");
        assert!(
            budget <= elapsed && elapsed < Duration::from_secs(5),
            "bounded by the budget: {elapsed:?}"
        );
        let sleeper = pid.load(Ordering::Relaxed);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while alive(sleeper) && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(!alive(sleeper), "the probe killed + reaped its child (C1)");
    }

    /// The pre-clean liveness check (C1): a stale file is not live
    /// (ECONNREFUSED), a listening path is -- both inside LIVE_TIMEOUT.
    #[tokio::test]
    async fn live_answers_listening_yes_stale_no() {
        let socket = tmp_dir("live").join("live.sock");
        assert!(!live(&socket).await, "no file: not live");
        let _listener = UnixListener::bind(&socket).expect("bind");
        let start = std::time::Instant::now();
        assert!(live(&socket).await, "a listener is live");
        assert!(start.elapsed() < LIVE_TIMEOUT);
        let _ = std::fs::remove_file(&socket);
    }
}
