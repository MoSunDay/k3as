//! containerd child supervision (TODO **T4.1**, decision **Q25**).
//!
//! Spawns the staged containerd binary, gates health on the CRI socket
//! accepting connections, restarts with exponential backoff when the child
//! dies (k3s `pkg/agent/containerd` supervise parity), and drains (SIGKILL
//! after a bounded wait — no libc dep under `#![forbid(unsafe_code)]`) when
//! the process-wide [`Shutdown`] fires.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use infra::Shutdown;
use tokio::net::UnixStream;
use tokio::process::{Child, Command};
use tokio::sync::watch;

/// Backoff base for the first restart.
pub const BASE_BACKOFF: Duration = Duration::from_millis(250);
/// Backoff ceiling.
pub const MAX_BACKOFF: Duration = Duration::from_secs(5);
/// A run this healthy resets the backoff ladder.
pub const STABLE_AFTER: Duration = Duration::from_secs(30);
/// How long a fresh child gets before its socket must accept connections.
pub const SOCKET_TIMEOUT: Duration = Duration::from_secs(15);
/// Bound on the final kill/await drain.
pub const DRAIN_GRACE: Duration = Duration::from_secs(10);

/// Everything the supervisor needs to run one containerd.
#[derive(Debug, Clone)]
pub struct SupervisorSpec {
    /// Staged containerd binary path.
    pub binary: PathBuf,
    /// Rendered `config.toml` (Q25 templating).
    pub config_path: PathBuf,
    /// Health gate: the socket the child must serve.
    pub socket: PathBuf,
    /// Directory prepended to the child's `PATH` (shims + runc live beside
    /// the binary; containerd resolves shims from its own dir, runc via PATH).
    pub path_prefix: Option<PathBuf>,
    /// containerd log level.
    pub log_level: String,
    pub socket_timeout: Duration,
    pub stable_after: Duration,
}

impl SupervisorSpec {
    pub fn new(
        binary: impl Into<PathBuf>,
        config_path: impl Into<PathBuf>,
        socket: impl Into<PathBuf>,
    ) -> Self {
        Self {
            binary: binary.into(),
            config_path: config_path.into(),
            socket: socket.into(),
            path_prefix: None,
            log_level: "warn".to_string(),
            socket_timeout: SOCKET_TIMEOUT,
            stable_after: STABLE_AFTER,
        }
    }
}

/// Exponential backoff: `base << restarts`, clamped to `cap`.
pub fn backoff_delay(restarts: u32, base: Duration, cap: Duration) -> Duration {
    let shift = restarts.min(16);
    base.saturating_mul(1u32 << shift).min(cap)
}

/// argv handed to the child (k3s parity: the config file carries the paths).
pub fn supervisor_args(spec: &SupervisorSpec) -> Vec<String> {
    vec![
        "-c".to_string(),
        spec.config_path.display().to_string(),
        "--log-level".to_string(),
        spec.log_level.clone(),
    ]
}

fn spawn_child(spec: &SupervisorSpec) -> std::io::Result<Child> {
    let mut cmd = Command::new(&spec.binary);
    cmd.args(supervisor_args(spec));
    // The child's logs flow into the agent's stderr (visible in golden logs).
    cmd.stdout(Stdio::inherit()).stderr(Stdio::inherit());
    cmd.kill_on_drop(true);
    if let Some(prefix) = &spec.path_prefix {
        let path = std::env::var("PATH").unwrap_or_default();
        cmd.env("PATH", format!("{}:{}", prefix.display(), path));
    }
    cmd.spawn()
}

/// Poll until the socket accepts a connection or the deadline passes.
/// `connect()` succeeding means the daemon is serving, not just that the
/// socket file exists.
pub async fn wait_socket(path: &Path, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if UnixStream::connect(path).await.is_ok() {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Final stats of one supervision session.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SuperviseStats {
    /// Restarts after unexpected exits (backoff-ladder position at the end).
    pub restarts: u32,
    /// Whether the socket ever went healthy under this supervisor.
    pub ever_healthy: bool,
}

/// SIGKILL the child and bound the wait (see module docs for the no-SIGTERM
/// simplification, recorded in Q25).
async fn drain_child(child: &mut Child) {
    let _ = child.start_kill();
    let _ = tokio::time::timeout(DRAIN_GRACE, child.wait()).await;
}

/// Supervise until `shutdown` fires, reporting the live child pid on `pid_tx`
/// (`None` once drained). Runs forever otherwise (restart loop).
pub async fn supervise(
    spec: SupervisorSpec,
    shutdown: Shutdown,
    pid_tx: watch::Sender<Option<u32>>,
) -> SuperviseStats {
    let mut stats = SuperviseStats::default();
    let mut restarts: u32 = 0;

    'supervise: loop {
        let mut child = match spawn_child(&spec) {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(
                    target: "init-pro",
                    binary = %spec.binary.display(),
                    "containerd spawn failed: {e}"
                );
                break 'supervise;
            }
        };
        let _ = pid_tx.send(child.id());
        tracing::info!(
            target: "init-pro",
            binary = %spec.binary.display(),
            pid = child.id(),
            "containerd child started"
        );

        // Phase 1 — become healthy: race socket readiness vs early exit.
        let healthy = 'health: {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    drain_child(&mut child).await;
                    break 'supervise;
                }
                res = child.wait() => {
                    match res {
                        Ok(st) => tracing::warn!(
                            target: "init-pro",
                            status = %st,
                            "containerd exited before healthy; restarting"
                        ),
                        Err(e) => tracing::warn!(
                            target: "init-pro",
                            "containerd wait failed pre-health: {e}"
                        ),
                    }
                    break 'health false;
                }
                ready = wait_socket(&spec.socket, spec.socket_timeout) => {
                    if ready { break 'health true; }
                    tracing::warn!(
                        target: "init-pro",
                        socket = %spec.socket.display(),
                        timeout_s = spec.socket_timeout.as_secs(),
                        "containerd socket never became healthy; recycling child"
                    );
                    drain_child(&mut child).await;
                    break 'health false;
                }
            }
        };

        if healthy {
            if !stats.ever_healthy {
                stats.ever_healthy = true;
                tracing::info!(
                    target: "init-pro",
                    socket = %spec.socket.display(),
                    "containerd healthy"
                );
            }
            // Phase 2 — monitor: hold the child until it dies, the run is
            // stable (backoff reset), or shutdown fires.
            let stable_timer = tokio::time::sleep(spec.stable_after);
            tokio::pin!(stable_timer);
            let mut stable = false;
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => {
                        drain_child(&mut child).await;
                        break 'supervise;
                    }
                    _ = &mut stable_timer, if !stable => {
                        stable = true;
                        restarts = 0;
                    }
                    res = child.wait() => {
                        match res {
                            Ok(st) => tracing::warn!(
                                target: "init-pro",
                                status = %st,
                                stable,
                                "containerd exited; restarting"
                            ),
                            Err(e) => tracing::warn!(
                                target: "init-pro",
                                "containerd wait failed: {e}"
                            ),
                        }
                        restarts = if stable { 1 } else { restarts + 1 };
                        break;
                    }
                }
            }
        } else {
            restarts += 1;
        }

        stats.restarts = restarts;
        let delay = backoff_delay(restarts, BASE_BACKOFF, MAX_BACKOFF);
        tracing::warn!(
            target: "init-pro",
            restarts,
            backoff_ms = delay.as_millis() as u64,
            "backing off before containerd restart"
        );
        tokio::select! {
            _ = shutdown.cancelled() => break 'supervise,
            _ = tokio::time::sleep(delay) => {}
        }
    }

    let _ = pid_tx.send(None);
    tracing::info!(target: "init-pro", restarts = stats.restarts, "containerd supervisor drained");
    stats
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_then_caps() {
        assert_eq!(
            backoff_delay(0, BASE_BACKOFF, MAX_BACKOFF),
            Duration::from_millis(250)
        );
        assert_eq!(
            backoff_delay(1, BASE_BACKOFF, MAX_BACKOFF),
            Duration::from_millis(500)
        );
        assert_eq!(
            backoff_delay(2, BASE_BACKOFF, MAX_BACKOFF),
            Duration::from_millis(1000)
        );
        assert_eq!(backoff_delay(5, BASE_BACKOFF, MAX_BACKOFF), MAX_BACKOFF);
        // Saturating: enormous restart counts clamp instead of panicking.
        assert_eq!(
            backoff_delay(u32::MAX, BASE_BACKOFF, MAX_BACKOFF),
            MAX_BACKOFF
        );
    }

    #[test]
    fn supervisor_args_pass_config_and_log_level() {
        let spec = SupervisorSpec::new("/x/containerd", "/y/config.toml", "/z/sock");
        assert_eq!(
            supervisor_args(&spec),
            vec!["-c", "/y/config.toml", "--log-level", "warn"]
        );
    }

    #[tokio::test]
    async fn wait_socket_times_out_when_nothing_listens() {
        let missing = std::env::temp_dir().join(format!("no-sock-{}", std::process::id()));
        assert!(!wait_socket(&missing, Duration::from_millis(300)).await);
    }

    #[tokio::test]
    async fn wait_socket_resolves_when_a_listener_appears() {
        let dir = std::env::temp_dir().join(format!("sock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("probe.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        // connect() against a bound listener succeeds immediately.
        assert!(wait_socket(&path, Duration::from_secs(2)).await);
        drop(listener);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
