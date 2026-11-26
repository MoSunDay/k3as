//! containerd child supervision (TODO **T4.1**, decision **Q25**).
//!
//! Spawns the staged containerd binary, gates health on the CRI socket
//! accepting connections, restarts with exponential backoff when the child
//! dies (k3s `pkg/agent/containerd` supervise parity), and drains (SIGKILL
//! after a bounded wait — no libc dep under `#![forbid(unsafe_code)]`) when
//! the process-wide [`Shutdown`] fires.
//!
//! Item **C12**: a spawn failure is retried on the same ladder unless it is
//! fatal ([`spawn_error_is_fatal`]: the bundle itself is broken), and the
//! session always ends with a logged terminal [`SuperviseStats`] carrying the
//! [`SuperviseExit`] reason, because the wiring level drops the task result.

use std::io;
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
    /// Extra `KEY=VALUE` env applied to the child on top of the inherited
    /// environment (T4.3 Scope A, Q34: `FLANNEL_SUBNET_FILE` points the
    /// flannel CNI meta-plugin at flanneld's subnet.env — CNI plugins
    /// inherit containerd's env, the Rust NetConf has no `subnetFile` key).
    pub extra_env: Vec<(String, String)>,
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
            extra_env: Vec::new(),
            log_level: "warn".to_string(),
            socket_timeout: SOCKET_TIMEOUT,
            stable_after: STABLE_AFTER,
        }
    }
}

/// The child environment, purely computed (unit-testable): inherited PATH
/// (optionally prefixed), then `extra_env` on top (later wins).
pub fn resolved_env(spec: &SupervisorSpec) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = Vec::new();
    let path = std::env::var("PATH").unwrap_or_default();
    match &spec.path_prefix {
        Some(prefix) => env.push(("PATH".to_string(), format!("{}:{}", prefix.display(), path))),
        None => env.push(("PATH".to_string(), path)),
    }
    for (k, v) in &spec.extra_env {
        env.push((k.clone(), v.clone()));
    }
    env
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

/// A spawn failure that retrying cannot fix (item C12). Transient kernel
/// pressure (`EAGAIN`/`ENOMEM` under fork storm -> `WouldBlock` /
/// `OutOfMemory`) is NOT fatal: the supervisor backs off and retries. A
/// missing binary (`NotFound`), an unexecutable one (`PermissionDenied`) or a
/// malformed argv/env (`InvalidInput`) describe a broken bundle: retrying
/// would only mask it, so supervision stops and the caller sees the reason.
pub fn spawn_error_is_fatal(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied | io::ErrorKind::InvalidInput
    )
}

/// Pure decision for one failed spawn (item C12): `None` = fatal, stop
/// supervising; `Some((next_restarts, delay))` = transient, sleep `delay`
/// and respawn. The failed attempt counts as a restart so the existing
/// 250 ms -> 5 s ladder ([`backoff_delay`]) paces the retries.
pub fn spawn_retry_plan(restarts: u32, err: &io::Error) -> Option<(u32, Duration)> {
    if spawn_error_is_fatal(err) {
        return None;
    }
    let next = restarts.saturating_add(1);
    Some((next, backoff_delay(next, BASE_BACKOFF, MAX_BACKOFF)))
}

fn spawn_child(spec: &SupervisorSpec) -> io::Result<Child> {
    let mut cmd = Command::new(&spec.binary);
    cmd.args(supervisor_args(spec));
    // The child's logs flow into the agent's stderr (visible in golden logs).
    cmd.stdout(Stdio::inherit()).stderr(Stdio::inherit());
    cmd.kill_on_drop(true);
    for (key, value) in resolved_env(spec) {
        cmd.env(key, value);
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

/// Why one supervision session ended (item C12): the supervisor's own
/// terminal log line and the returned [`SuperviseStats`] carry this, so a
/// containerd that is DOWN is observable even when the wiring level drops the
/// task result.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SuperviseExit {
    /// The process-wide [`Shutdown`] fired and the child was drained: the
    /// normal, expected end.
    #[default]
    Shutdown,
    /// The bundle is broken ([`spawn_error_is_fatal`]); containerd is DOWN
    /// and stays down until the agent itself is restarted.
    FatalSpawn,
}

/// Final stats of one supervision session.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SuperviseStats {
    /// Restarts after unexpected exits (backoff-ladder position at the end).
    pub restarts: u32,
    /// Whether the socket ever went healthy under this supervisor.
    pub ever_healthy: bool,
    /// How the session ended (item C12).
    pub exit: SuperviseExit,
}

/// SIGKILL the child and bound the wait (see module docs for the no-SIGTERM
/// simplification, recorded in Q25).
async fn drain_child(child: &mut Child) {
    let _ = child.start_kill();
    let _ = tokio::time::timeout(DRAIN_GRACE, child.wait()).await;
}

/// Supervise until `shutdown` fires, reporting the live child pid on `pid_tx`
/// (`None` once drained). Runs forever otherwise (restart loop).
///
/// Item C12: a transient spawn failure is retried on the backoff ladder;
/// only a fatal one ([`spawn_error_is_fatal`]) ends the session. The terminal
/// [`SuperviseStats`] (restarts + [`SuperviseExit`]) is logged before
/// returning, because the caller may drop the task result.
pub async fn supervise(
    spec: SupervisorSpec,
    shutdown: Shutdown,
    pid_tx: watch::Sender<Option<u32>>,
) -> SuperviseStats {
    supervise_with(spec, shutdown, pid_tx, spawn_child).await
}

/// [`supervise`] with the spawn step injected: item C12 needs to exercise the
/// transient-retry path without provoking a real `EAGAIN` from the kernel.
/// Production always passes [`spawn_child`].
async fn supervise_with<F>(
    spec: SupervisorSpec,
    shutdown: Shutdown,
    pid_tx: watch::Sender<Option<u32>>,
    spawn: F,
) -> SuperviseStats
where
    F: Fn(&SupervisorSpec) -> io::Result<Child>,
{
    let mut stats = SuperviseStats::default();
    let mut restarts: u32 = 0;
    let mut exit = SuperviseExit::Shutdown;

    'supervise: loop {
        // Item C12: a spawn Err is NOT automatically fatal. Transient kernel
        // pressure (EAGAIN/ENOMEM under fork storm) used to `break` here,
        // which killed containerd forever while the agent stayed "Ready";
        // now it is retried on the existing backoff ladder and stays
        // shutdown-cancellable.
        let mut child = match spawn(&spec) {
            Ok(c) => c,
            Err(e) => match spawn_retry_plan(restarts, &e) {
                None => {
                    tracing::error!(
                        target: "init-pro",
                        binary = %spec.binary.display(),
                        error = %e,
                        kind = ?e.kind(),
                        restarts,
                        "containerd spawn failed fatally; giving up (bundle is broken, C12)"
                    );
                    exit = SuperviseExit::FatalSpawn;
                    break 'supervise;
                }
                Some((next, delay)) => {
                    restarts = next;
                    stats.restarts = restarts;
                    tracing::error!(
                        target: "init-pro",
                        binary = %spec.binary.display(),
                        error = %e,
                        kind = ?e.kind(),
                        restarts,
                        backoff_ms = delay.as_millis() as u64,
                        "containerd spawn failed transiently; retrying (C12)"
                    );
                    tokio::select! {
                        _ = shutdown.cancelled() => break 'supervise,
                        _ = tokio::time::sleep(delay) => {}
                    }
                    continue 'supervise;
                }
            },
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
                        // Saturating: a long-lived supervisor never panics
                        // on the u32 ladder (C12 hardened the same arithmetic
                        // on the spawn-retry path).
                        restarts = if stable { 1 } else { restarts.saturating_add(1) };
                        break;
                    }
                }
            }
        } else {
            restarts = restarts.saturating_add(1);
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
    stats.exit = exit;
    // Item C12: the supervisor logs its own terminal stats because the
    // wiring level (`cli`) drops the spawned task's result, so a DOWN
    // containerd must still leave an operator-visible line behind.
    match exit {
        SuperviseExit::Shutdown => tracing::info!(
            target: "init-pro",
            restarts = stats.restarts,
            ever_healthy = stats.ever_healthy,
            exit = ?SuperviseExit::Shutdown,
            "containerd supervisor drained"
        ),
        SuperviseExit::FatalSpawn => tracing::warn!(
            target: "init-pro",
            binary = %spec.binary.display(),
            restarts = stats.restarts,
            ever_healthy = stats.ever_healthy,
            exit = ?SuperviseExit::FatalSpawn,
            "containerd supervisor exited WITHOUT draining a live child: \
             containerd is down until the agent restarts (C12)"
        ),
    }
    stats
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn resolved_env_applies_prefix_and_extra() {
        let mut spec = SupervisorSpec::new("/bin/containerd", "/etc/c.toml", "/run/c.sock");
        spec.path_prefix = Some(PathBuf::from("/dd/agent/containerd"));
        spec.extra_env = vec![(
            "FLANNEL_SUBNET_FILE".to_string(),
            "/dd/agent/flannel/subnet.env".to_string(),
        )];
        let env = resolved_env(&spec);
        let path = env.iter().find(|(k, _)| k == "PATH").unwrap();
        assert!(
            path.1.starts_with("/dd/agent/containerd:"),
            "prefix ahead of inherited PATH: {path:?}"
        );
        assert!(
            env.contains(&(
                "FLANNEL_SUBNET_FILE".to_string(),
                "/dd/agent/flannel/subnet.env".to_string()
            )),
            "extra env carried to the child: {env:?}"
        );
    }

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

    // ---- item C12: spawn-failure classification + retry plan ----

    #[test]
    fn spawn_error_is_fatal_only_for_a_broken_bundle() {
        // Broken bundle: retrying would mask the real fault.
        for kind in [
            io::ErrorKind::NotFound,
            io::ErrorKind::PermissionDenied,
            io::ErrorKind::InvalidInput,
        ] {
            let err = io::Error::new(kind, "probe");
            assert!(spawn_error_is_fatal(&err), "{kind:?} must be fatal");
        }
        // Transient kernel pressure: the supervisor MUST retry these.
        for kind in [
            io::ErrorKind::WouldBlock,
            io::ErrorKind::OutOfMemory,
            io::ErrorKind::Interrupted,
            io::ErrorKind::TimedOut,
            io::ErrorKind::ConnectionReset,
            io::ErrorKind::Other,
        ] {
            let err = io::Error::new(kind, "probe");
            assert!(!spawn_error_is_fatal(&err), "{kind:?} must be retryable");
        }
    }

    #[test]
    fn spawn_error_is_fatal_maps_real_os_errnos() {
        // ENOENT / EACCES / EINVAL are the "bundle is broken" family;
        // EAGAIN / ENOMEM are the fork-pressure family (C12).
        assert!(spawn_error_is_fatal(&io::Error::from_raw_os_error(2)));
        assert!(spawn_error_is_fatal(&io::Error::from_raw_os_error(13)));
        assert!(spawn_error_is_fatal(&io::Error::from_raw_os_error(22)));
        assert!(!spawn_error_is_fatal(&io::Error::from_raw_os_error(11)));
        assert!(!spawn_error_is_fatal(&io::Error::from_raw_os_error(12)));
    }

    #[test]
    fn spawn_retry_plan_counts_the_failure_as_a_restart() {
        let transient = io::Error::new(io::ErrorKind::OutOfMemory, "fork");
        assert_eq!(
            spawn_retry_plan(0, &transient),
            Some((1, Duration::from_millis(500)))
        );
        assert_eq!(
            spawn_retry_plan(3, &transient),
            Some((4, Duration::from_secs(4)))
        );
        // Same ceiling as the ordinary restart ladder.
        assert_eq!(spawn_retry_plan(9, &transient), Some((10, MAX_BACKOFF)));
        // Saturating instead of panicking on a very long session.
        assert_eq!(
            spawn_retry_plan(u32::MAX, &transient),
            Some((u32::MAX, MAX_BACKOFF))
        );
    }

    #[test]
    fn spawn_retry_plan_stops_on_fatal_errors() {
        let fatal = io::Error::new(io::ErrorKind::NotFound, "no containerd");
        assert_eq!(spawn_retry_plan(0, &fatal), None);
        assert_eq!(spawn_retry_plan(7, &fatal), None);
    }

    #[tokio::test]
    async fn supervise_reports_fatal_spawn_and_returns() {
        // C12: a broken bundle still stops the loop (no infinite retry), but
        // the exit reason is observable in both the stats and the pid watch.
        let dd = std::env::temp_dir().join(format!("rt-fatal-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dd);
        std::fs::create_dir_all(&dd).unwrap();
        let spec = SupervisorSpec::new(
            dd.join("no-such-containerd"),
            dd.join("config.toml"),
            dd.join("containerd.sock"),
        );
        let shutdown = Shutdown::new();
        let (pid_tx, mut pid_rx) = watch::channel::<Option<u32>>(None);
        let stats = tokio::time::timeout(Duration::from_secs(5), supervise(spec, shutdown, pid_tx))
            .await
            .expect("fatal spawn must not loop forever");
        assert_eq!(stats.exit, SuperviseExit::FatalSpawn);
        assert!(!stats.ever_healthy);
        assert_eq!(stats.restarts, 0, "a fatal attempt never burns the ladder");
        assert_eq!(*pid_rx.borrow(), None, "no live child pid was published");
        // The sender publishes a final `None` and is then dropped: the watch
        // closes instead of staying pending, so a waiter can observe the exit.
        let closed = tokio::time::timeout(Duration::from_secs(1), pid_rx.changed())
            .await
            .expect("pid watch closes when the supervisor exits (C12)");
        // Ok(()) = the final `None` send; Err = the sender drop. Either way
        // the value a waiter sees is "no live child".
        let _ = closed;
        assert_eq!(*pid_rx.borrow(), None);
        let _ = std::fs::remove_dir_all(&dd);
    }

    #[tokio::test]
    async fn supervise_retries_a_transient_spawn_failure() {
        // C12 regression: before the fix the first spawn Err broke the loop
        // and containerd stayed dead while the agent looked "Ready".
        let dd = std::env::temp_dir().join(format!("rt-retry-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dd);
        std::fs::create_dir_all(&dd).unwrap();
        let mut spec = SupervisorSpec::new(
            dd.join("containerd"),
            dd.join("config.toml"),
            dd.join("containerd.sock"),
        );
        // No socket will ever listen here: the health race decides first.
        spec.socket_timeout = Duration::from_millis(50);
        spec.stable_after = Duration::from_millis(50);
        let shutdown = Shutdown::new();
        let (pid_tx, mut pid_rx) = watch::channel::<Option<u32>>(None);

        // Fail twice with fork-pressure errors, then hand over a real child.
        let attempts = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let counter = Arc::clone(&attempts);
        let spawn = move |_s: &SupervisorSpec| {
            let n = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if n < 2 {
                return Err(io::Error::new(io::ErrorKind::OutOfMemory, "fork pressure"));
            }
            Command::new("sleep").arg("30").kill_on_drop(true).spawn()
        };
        let task = tokio::spawn(supervise_with(spec, shutdown.clone(), pid_tx, spawn));

        // The child pid only shows up because the transient errors retried.
        let pid = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Some(pid) = *pid_rx.borrow_and_update() {
                    return pid;
                }
                let _ = pid_rx.changed().await;
            }
        })
        .await
        .expect("supervisor must retry a transient spawn failure (C12)");

        shutdown.trigger();
        let stats = tokio::time::timeout(Duration::from_secs(20), task)
            .await
            .expect("supervisor drains on shutdown")
            .expect("supervisor task joins");
        assert!(
            attempts.load(std::sync::atomic::Ordering::SeqCst) >= 3,
            "two transient failures then a real spawn: {}",
            attempts.load(std::sync::atomic::Ordering::SeqCst)
        );
        assert_eq!(stats.exit, SuperviseExit::Shutdown);
        assert!(
            stats.restarts >= 2,
            "each transient spawn failure counts as a restart: {}",
            stats.restarts
        );
        assert!(!proc_exists(pid), "the child was drained on shutdown");
        let _ = std::fs::remove_dir_all(&dd);
    }

    /// `/proc/<pid>` presence probe (Linux) for the drain assertions.
    fn proc_exists(pid: u32) -> bool {
        Path::new(&format!("/proc/{pid}")).exists()
    }
}
