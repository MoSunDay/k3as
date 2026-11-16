//! Probe engine (TODO **T4.2** Sprint 20 S1 + TODO **T4.8** Sprint 39):
//! startup/readiness/liveness probes with upstream threshold semantics.
//! [`Tracker`] is a pure state machine (consecutive counters);
//! [`PodProbeState`] feeds the restart planner (`liveness_failed`) and
//! status writer (`is_ready`).
//!
//! T4.8 completes the handler set: `httpGet` (Sprint 20) plus
//! `tcpSocket` (connect-then-close, numeric port) and `exec` (argv
//! verbatim, rc==0 == healthy) — upstream `pkg/kubelet/prober` parity.
//! Startup failure does NOT restart — it only gates readiness/liveness.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use serde_json::Value;

use crate::http::HttpJson;

/// Which probe kind a tracker belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeKind {
    Startup,
    Readiness,
    Liveness,
}

/// `httpGet` action: port is a NUMBER in S1 (IntOrString deferred),
/// missing port defaults to 80.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpGetSpec {
    pub path: String,
    pub port: u16,
}

/// `tcpSocket` action (T4.8): numeric port only — named ports
/// (IntOrString) are **Deferred** with the httpGet port, upstream
/// `pkg/kubelet/prober/prober.go` TCP semantics: success is "connect
/// established", then close immediately (no data exchange).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TcpSocketSpec {
    pub port: u16,
}

/// `exec` action (T4.8): argv words passed VERBATIM to the container —
/// no shell wrapping (k8s parity, upstream `pkg/kubelet/prober/exec`);
/// healthy iff the process exits 0.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecSpec {
    pub command: Vec<String>,
}

/// The probe handler: exactly one of `httpGet` / `tcpSocket` / `exec`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeHandler {
    HttpGet(HttpGetSpec),
    TcpSocket(TcpSocketSpec),
    Exec(ExecSpec),
}

/// One probe definition, upstream defaults applied at parse time (period
/// 10s, timeout 1s, failureThreshold 3, successThreshold 1, delay 0, "/").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeSpec {
    pub handler: ProbeHandler,
    pub initial_delay_seconds: u32,
    pub period_seconds: u32,
    pub timeout_seconds: u32,
    pub failure_threshold: u32,
    pub success_threshold: u32,
}

impl ProbeSpec {
    /// Parse one probe object (`startupProbe`/`readinessProbe`/
    /// `livenessProbe`); `None` without a recognized, well-formed
    /// handler (httpGet / tcpSocket / exec, T4.8).
    pub fn from_json(v: &Value) -> Option<ProbeSpec> {
        let handler = parse_handler(v)?;
        let num = |k: &str, d: u32| {
            v.get(k)
                .and_then(Value::as_u64)
                .map(|x| x.min(u64::from(u32::MAX)) as u32)
                .unwrap_or(d)
        };
        Some(ProbeSpec {
            handler,
            initial_delay_seconds: num("initialDelaySeconds", 0),
            period_seconds: num("periodSeconds", 10),
            timeout_seconds: num("timeoutSeconds", 1),
            failure_threshold: num("failureThreshold", 3),
            success_threshold: num("successThreshold", 1),
        })
    }
}

/// Recognized handler keys, first match wins (`httpGet` keeps S1
/// precedence when several are present — upstream validates exactly
/// one via API schema; we are lenient readers).
fn parse_handler(v: &Value) -> Option<ProbeHandler> {
    if let Some(get) = v.get("httpGet").and_then(Value::as_object) {
        let port = get
            .get("port")
            .and_then(Value::as_u64)
            .map(|p| p.min(u64::from(u16::MAX)) as u16)
            .unwrap_or(80);
        let path = get
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or("/")
            .to_string();
        return Some(ProbeHandler::HttpGet(HttpGetSpec { path, port }));
    }
    if let Some(tcp) = v.get("tcpSocket").and_then(Value::as_object) {
        // Numeric port only; a NAMED port (string) is Deferred with
        // IntOrString -> unrecognized handler -> None (T4.8 scope note).
        let port = tcp.get("port").and_then(Value::as_u64)?;
        return Some(ProbeHandler::TcpSocket(TcpSocketSpec {
            port: port.min(u64::from(u16::MAX)) as u16,
        }));
    }
    if let Some(ex) = v.get("exec").and_then(Value::as_object) {
        let command = ex
            .get("command")?
            .as_array()?
            .iter()
            .map(|w| w.as_str().map(str::to_string))
            .collect::<Option<Vec<String>>>()?;
        if command.is_empty() {
            return None; // upstream requires a non-empty argv
        }
        return Some(ProbeHandler::Exec(ExecSpec { command }));
    }
    None
}

/// Outcome of one probe: unknown until thresholds are crossed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeStatus {
    Unknown,
    Success,
    Failure,
}

/// Per-(pod, container, kind) bookkeeping: status, consecutive counters,
/// and next probe time in unix seconds (start + initialDelay, then +period).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tracker {
    pub status: ProbeStatus,
    pub consecutive_successes: u32,
    pub consecutive_failures: u32,
    /// Unix seconds of the next probe; `None` until the container is first
    /// observed RUNNING (then armed with `initialDelaySeconds`).
    pub next_probe_at: Option<u64>,
}

impl Tracker {
    pub fn new() -> Self {
        Self {
            status: ProbeStatus::Unknown,
            consecutive_successes: 0,
            consecutive_failures: 0,
            next_probe_at: None,
        }
    }

    /// Arm the first probe (idempotent): initialDelay after first RUNNING.
    pub fn arm(&mut self, now: u64, spec: &ProbeSpec) {
        if self.next_probe_at.is_none() {
            self.next_probe_at = Some(now + u64::from(spec.initial_delay_seconds));
        }
    }

    /// True when a probe is scheduled at or before `now`.
    pub fn due(&self, now: u64) -> bool {
        self.next_probe_at.is_some_and(|t| now >= t)
    }

    /// Fold one probe outcome in (upstream semantics): a failure flips to
    /// Unknown immediately (readiness drops), reaching `Failure` only
    /// after `failureThreshold` consecutive failures (liveness restart);
    /// a success reaches `Success` after `successThreshold` successes;
    /// the next probe runs `periodSeconds` after the outcome.
    pub fn observe(&mut self, ok: bool, now: u64, spec: &ProbeSpec) {
        self.next_probe_at = Some(now + u64::from(spec.period_seconds));
        if ok {
            self.consecutive_failures = 0;
            self.consecutive_successes += 1;
            self.status = if self.consecutive_successes >= spec.success_threshold {
                ProbeStatus::Success
            } else {
                ProbeStatus::Unknown
            };
        } else {
            self.consecutive_successes = 0;
            self.consecutive_failures += 1;
            self.status = if self.consecutive_failures >= spec.failure_threshold {
                ProbeStatus::Failure
            } else {
                ProbeStatus::Unknown
            };
        }
    }

    pub fn is_success(&self) -> bool {
        self.status == ProbeStatus::Success
    }
}

impl Default for Tracker {
    fn default() -> Self {
        Self::new()
    }
}

/// The three trackers of one (pod, container) pair; trackers exist only
/// for probe kinds the spec declares (None = no probe of that kind).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContainerProbeState {
    pub startup: Option<Tracker>,
    pub readiness: Option<Tracker>,
    pub liveness: Option<Tracker>,
}

impl ContainerProbeState {
    /// The tracker for `kind`, creating a fresh one on first use.
    pub fn tracker(&mut self, kind: ProbeKind) -> &mut Tracker {
        let slot = match kind {
            ProbeKind::Startup => &mut self.startup,
            ProbeKind::Readiness => &mut self.readiness,
            ProbeKind::Liveness => &mut self.liveness,
        };
        slot.get_or_insert_with(Tracker::new)
    }

    /// Startup gates the other two: true when there is no startup probe or
    /// it succeeded.
    pub fn is_startup_ok(&self) -> bool {
        self.startup.as_ref().is_none_or(Tracker::is_success)
    }

    /// Probe-side readiness (the status writer ANDs this with the CRI
    /// RUNNING state): no readiness probe or Success, and startup ok.
    pub fn is_ready(&self) -> bool {
        self.readiness.as_ref().is_none_or(Tracker::is_success) && self.is_startup_ok()
    }

    /// Restart trigger: liveness Failure AND startup passed (startup
    /// failure does NOT restart in S1, only gates the others).
    pub fn liveness_failed(&self) -> bool {
        let failed = self
            .liveness
            .as_ref()
            .is_some_and(|t| t.status == ProbeStatus::Failure);
        failed && self.is_startup_ok()
    }
}

/// All probe trackers across desired pods, keyed by (pod_key,
/// container_name); a new incarnation (id/attempt changed) resets the
/// pair so restarts re-arm initialDelaySeconds instead of stale Failure.
#[derive(Debug, Clone, Default)]
pub struct PodProbeState {
    per_container: BTreeMap<(String, String), ContainerProbeState>,
    seen: BTreeMap<(String, String), (String, u32)>,
}

impl PodProbeState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Mutable access to one (pod, container) pair (created on demand).
    pub fn entry(&mut self, pod_key: &str, container: &str) -> &mut ContainerProbeState {
        self.per_container
            .entry((pod_key.to_string(), container.to_string()))
            .or_default()
    }

    fn get(&self, pod_key: &str, container: &str) -> Option<&ContainerProbeState> {
        self.per_container
            .get(&(pod_key.to_string(), container.to_string()))
    }

    /// Note the observed incarnation; trackers reset when it changed.
    pub fn note_incarnation(&mut self, pod_key: &str, container: &str, id: &str, attempt: u32) {
        let key = (pod_key.to_string(), container.to_string());
        if self.seen.get(&key) == Some(&(id.to_string(), attempt)) {
            return;
        }
        self.seen.insert(key.clone(), (id.to_string(), attempt));
        self.per_container
            .insert(key, ContainerProbeState::default());
    }

    /// Startup passed for the container (true when no startup probe).
    pub fn is_startup_ok(&self, pod_key: &str, container: &str) -> bool {
        self.get(pod_key, container)
            .is_none_or(ContainerProbeState::is_startup_ok)
    }

    /// Probe-side readiness for the container (true when no probes).
    pub fn is_ready(&self, pod_key: &str, container: &str) -> bool {
        self.get(pod_key, container)
            .is_none_or(ContainerProbeState::is_ready)
    }

    /// Liveness Failure AND startup passed -> the planner restarts the
    /// container (restartPolicy permitting) even while it is RUNNING.
    pub fn liveness_failed(&self, pod_key: &str, container: &str) -> bool {
        self.get(pod_key, container)
            .is_some_and(ContainerProbeState::liveness_failed)
    }

    /// True when any probe tracker of the container is due at `now`.
    pub fn next_probe_due(&self, pod_key: &str, container: &str, now: u64) -> bool {
        self.get(pod_key, container).is_some_and(|s| {
            [
                s.startup.as_ref(),
                s.readiness.as_ref(),
                s.liveness.as_ref(),
            ]
            .into_iter()
            .flatten()
            .any(|t| t.due(now))
        })
    }

    /// Drop trackers of pods no longer desired (teardown/prune).
    pub fn prune(&mut self, pod_keys: &BTreeSet<String>) {
        self.per_container.retain(|(k, _), _| pod_keys.contains(k));
        self.seen.retain(|(k, _), _| pod_keys.contains(k));
    }
}

/// Run one HTTP probe against `host:port_override` (Success = 2xx/3xx;
/// anything else, connect errors, and timeouts are failures as values).
/// Bounded by `timeout` (upstream `timeoutSeconds`, default 1s).
pub async fn run_http_probe(
    spec: &HttpGetSpec,
    host: &str,
    port_override: u16,
    timeout: Duration,
) -> Result<(), String> {
    let client = HttpJson::parse_url(&format!("http://{host}:{port_override}"))
        .map_err(|e| format!("probe target http://{host}:{port_override}: {e}"))?;
    let path = match spec.path.as_str() {
        "" => "/".to_string(),
        p if p.starts_with('/') => p.to_string(),
        p => format!("/{p}"),
    };
    match tokio::time::timeout(timeout, client.request("GET", &path, None)).await {
        Err(_) => Err(format!("probe GET {path} timed out after {timeout:?}")),
        Ok(Err(e)) => Err(format!("probe GET {path} failed: {e}")),
        Ok(Ok((code, _))) if (200..400).contains(&code) => Ok(()),
        Ok(Ok((code, _))) => Err(format!("probe GET {path} returned status {code}")),
    }
}

/// Run one TCP probe (T4.8): connect to `host:port` bounded by `timeout`
/// (upstream `timeoutSeconds`, default 1s). Success = connect
/// established, then the socket drops — "connect then close", upstream
/// `pkg/kubelet/prober` TCP semantics (no data exchanged).
pub async fn run_tcp_probe(host: &str, port: u16, timeout: Duration) -> Result<(), String> {
    match tokio::time::timeout(timeout, tokio::net::TcpStream::connect((host, port))).await {
        Err(_) => Err(format!(
            "probe TCP {host}:{port} timed out after {timeout:?}"
        )),
        Ok(Err(e)) => Err(format!("probe TCP {host}:{port} failed: {e}")),
        Ok(Ok(_stream)) => Ok(()), // connected: healthy; stream drops -> close
    }
}

/// Exec probe verdict (T4.8): healthy IFF the container process exited
/// 0 — upstream `pkg/kubelet/prober` exec semantics. Pure seam so the
/// verdict is unit-testable independent of the CRI exec transport (the
/// transport itself is e2e-covered, `scripts/probe-e2e.sh` P3/P4).
pub fn exec_probe_healthy(exit_code: i32) -> bool {
    exit_code == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn spec() -> ProbeSpec {
        ProbeSpec {
            handler: ProbeHandler::HttpGet(HttpGetSpec {
                path: "/readyz".into(),
                port: 8080,
            }),
            initial_delay_seconds: 0,
            period_seconds: 1,
            timeout_seconds: 1,
            failure_threshold: 3,
            success_threshold: 1,
        }
    }

    #[test]
    fn parse_applies_upstream_defaults() {
        let p = ProbeSpec::from_json(&json!({"httpGet": {"path": "/z"}})).unwrap();
        assert_eq!(
            p.handler,
            ProbeHandler::HttpGet(HttpGetSpec {
                path: "/z".into(),
                port: 80
            })
        );
        assert_eq!(
            (p.initial_delay_seconds, p.period_seconds, p.timeout_seconds),
            (0, 10, 1)
        );
        assert_eq!((p.failure_threshold, p.success_threshold), (3, 1));
        let p = ProbeSpec::from_json(&json!({"httpGet": {"port": 9}})).unwrap();
        assert_eq!(
            p.handler,
            ProbeHandler::HttpGet(HttpGetSpec {
                path: "/".into(),
                port: 9
            })
        );
    }

    #[test]
    fn parse_tcp_socket_numeric_port() {
        let p = ProbeSpec::from_json(&json!({"tcpSocket": {"port": 8080}})).unwrap();
        assert_eq!(
            p.handler,
            ProbeHandler::TcpSocket(TcpSocketSpec { port: 8080 })
        );
        assert_eq!(
            (p.period_seconds, p.timeout_seconds, p.failure_threshold),
            (10, 1, 3),
            "upstream defaults apply to every handler kind (T4.8)"
        );
        // Overrides still apply.
        let p = ProbeSpec::from_json(
            &json!({"tcpSocket": {"port": 9999}, "periodSeconds": 2, "failureThreshold": 2}),
        )
        .unwrap();
        assert_eq!(
            p.handler,
            ProbeHandler::TcpSocket(TcpSocketSpec { port: 9999 })
        );
        assert_eq!((p.period_seconds, p.failure_threshold), (2, 2));
    }

    #[test]
    fn parse_exec_command_verbatim() {
        let p = ProbeSpec::from_json(&json!({"exec": {"command": ["/bin/wfstep", "probe-ok"]}}))
            .unwrap();
        assert_eq!(
            p.handler,
            ProbeHandler::Exec(ExecSpec {
                command: vec!["/bin/wfstep".to_string(), "probe-ok".to_string()]
            })
        );
        // argv is taken verbatim: shell metacharacters stay data words.
        let p = ProbeSpec::from_json(&json!({"exec": {"command": ["a b", "|rm -rf"]}})).unwrap();
        match p.handler {
            ProbeHandler::Exec(ExecSpec { command }) => {
                assert_eq!(command, vec!["a b".to_string(), "|rm -rf".to_string()])
            }
            other => panic!("expected exec handler, got {other:?}"),
        }
    }

    #[test]
    fn parse_rejects_malformed_handlers() {
        for v in [
            json!({}),
            json!({"httpGet": null}),
            json!({"grpc": {"port": 1}}),
            json!(42),
            // Named (string) tcp port: IntOrString Deferred -> not parsed.
            json!({"tcpSocket": {"port": "http"}}),
            json!({"tcpSocket": {}}),
            // exec needs a non-empty array of plain strings.
            json!({"exec": {}}),
            json!({"exec": {"command": []}}),
            json!({"exec": {"command": [42]}}),
            json!({"exec": {"command": "not-an-array"}}),
        ] {
            assert!(
                ProbeSpec::from_json(&v).is_none(),
                "unrecognized/malformed handler -> none: {v}"
            );
        }
    }

    #[test]
    fn parse_http_get_wins_when_handlers_overlap() {
        // Lenient-reader precedence (S1 holdover): httpGet first.
        let p = ProbeSpec::from_json(
            &json!({"httpGet": {"port": 1}, "tcpSocket": {"port": 2}, "exec": {"command": ["x"]}}),
        )
        .unwrap();
        assert!(matches!(p.handler, ProbeHandler::HttpGet(_)));
        let p =
            ProbeSpec::from_json(&json!({"tcpSocket": {"port": 2}, "exec": {"command": ["x"]}}))
                .unwrap();
        assert!(matches!(p.handler, ProbeHandler::TcpSocket(_)));
    }

    #[test]
    fn exec_verdict_is_exit_code_zero_only() {
        assert!(exec_probe_healthy(0));
        assert!(!exec_probe_healthy(1));
        assert!(!exec_probe_healthy(127));
        assert!(!exec_probe_healthy(-1));
    }

    #[tokio::test]
    async fn tcp_probe_succeeds_on_open_listener() {
        // Bind an ephemeral loopback listener (the "container" side) and
        // accept in the background so the connect completes.
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let acceptor = tokio::spawn(async move {
            // "connect then close": the prober drops immediately; accept
            // errors (prober-side reset) are fine, just keep accepting.
            while listener.accept().await.is_ok() {}
        });
        assert!(
            run_tcp_probe("127.0.0.1", addr.port(), Duration::from_secs(1))
                .await
                .is_ok()
        );
        acceptor.abort();
    }

    #[tokio::test]
    async fn tcp_probe_fails_on_closed_port() {
        // A freshly-freed ephemeral port can be SNIPED: tokio tests run in
        // parallel, and a sibling bind((127.0.0.1, 0)) can land on the exact
        // port this test just dropped, turning the refused-connect into a
        // false success. Retry with fresh ports; only fail the assertion
        // once every attempt got answered (probability ~0 in practice).
        for _ in 0..8 {
            let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
                .await
                .unwrap();
            let addr = listener.local_addr().unwrap();
            drop(listener);
            match run_tcp_probe("127.0.0.1", addr.port(), Duration::from_secs(1)).await {
                Err(_) => return,   // refused, as a closed port must be
                Ok(()) => continue, // sniped between drop and connect
            }
        }
        panic!("closed port must fail: connect answered on 8 fresh ports");
    }

    #[tokio::test]
    async fn tcp_probe_timeout_is_a_failure() {
        // A bound-but-unaccepted socket still completes the TCP handshake
        // (kernel backlog), so a true timeout needs a black hole: probe an
        // unroutable TEST-NET address with a tiny budget -> timeout branch.
        let res = run_tcp_probe("192.0.2.1", 9999, Duration::from_millis(150)).await;
        assert!(res.is_err(), "unreachable target must fail: {res:?}");
    }

    #[test]
    fn thresholds_need_consecutive_outcomes() {
        let mut t = Tracker::new();
        for i in 1..=3 {
            t.observe(false, i, &spec());
            assert_eq!(
                t.status,
                if i < 3 {
                    ProbeStatus::Unknown
                } else {
                    ProbeStatus::Failure
                }
            );
        }
        t.observe(true, 4, &spec());
        assert_eq!(
            (t.status, t.consecutive_failures),
            (ProbeStatus::Success, 0)
        );
        let mut s = spec();
        s.success_threshold = 2;
        let mut t = Tracker::new();
        t.observe(true, 1, &s);
        assert_eq!(t.status, ProbeStatus::Unknown);
        t.observe(true, 2, &s);
        assert_eq!(t.status, ProbeStatus::Success);
        t.observe(false, 3, &s);
        assert_eq!(t.status, ProbeStatus::Unknown, "a failure resets successes");
        assert_eq!(t.consecutive_successes, 0);
    }

    #[test]
    fn startup_gates_ready_and_liveness() {
        let mut state = ContainerProbeState::default();
        assert!(state.is_startup_ok() && state.is_ready() && !state.liveness_failed());
        // failureThreshold 3: one failure -> Unknown (not ok / no restart).
        state.tracker(ProbeKind::Startup).observe(false, 1, &spec());
        state
            .tracker(ProbeKind::Liveness)
            .observe(false, 2, &spec());
        assert!(!state.is_startup_ok() && !state.liveness_failed());
        state.tracker(ProbeKind::Startup).observe(true, 3, &spec());
        assert!(state.is_startup_ok());
        // Fold liveness to Failure (3 consecutive) while startup is ok.
        (4..=6).for_each(|i| {
            state
                .tracker(ProbeKind::Liveness)
                .observe(false, i, &spec())
        });
        assert!(state.liveness_failed(), "startup passed + liveness failed");
        // One readiness failure (below failureThreshold) drops ready already.
        state
            .tracker(ProbeKind::Readiness)
            .observe(false, 7, &spec());
        assert!(!state.is_ready(), "readiness failure gates ready");
        state
            .tracker(ProbeKind::Readiness)
            .observe(true, 8, &spec());
        assert!(state.is_ready());
    }

    #[test]
    fn scheduling_arms_initial_delay_then_period() {
        let mut t = Tracker::new();
        let mut s = spec();
        s.initial_delay_seconds = 5;
        t.arm(100, &s);
        assert!(!t.due(104) && t.due(105), "first probe after initial delay");
        t.observe(true, 105, &s); // period 1
        assert!(t.due(106) && !t.due(105), "next probe a period later");
        t.arm(1000, &s);
        assert!(t.due(106), "arm must not move an armed tracker");
    }

    #[test]
    fn incarnation_change_resets_and_prune_drops() {
        let mut probes = PodProbeState::new();
        probes.note_incarnation("ns/p", "c0", "id1", 0);
        let t = probes.entry("ns/p", "c0").tracker(ProbeKind::Liveness);
        (1..=3).for_each(|i| t.observe(false, i, &spec()));
        assert!(probes.liveness_failed("ns/p", "c0"));
        probes.note_incarnation("ns/p", "c0", "id1", 0);
        assert!(
            probes.liveness_failed("ns/p", "c0"),
            "same incarnation keeps failure"
        );
        probes.note_incarnation("ns/p", "c0", "id2", 1);
        assert!(
            !probes.liveness_failed("ns/p", "c0")
                && !probes.next_probe_due("ns/p", "c0", 1_000_000),
            "new incarnation resets failure + re-arms"
        );
        probes.prune(&["ns/p"].iter().map(|s| s.to_string()).collect());
        assert!(probes.is_ready("ns/p", "c0"), "kept pod's trackers survive");
        probes.prune(&BTreeSet::new());
        assert!(
            !probes.next_probe_due("ns/p", "c0", 0),
            "pruned pod drops trackers"
        );
    }
}
