//! Embedded flannel wiring (TODO **T4.3 Scope A**, decision **Q34**):
//! net-conf.json construction + flanneld (kube subnet-mgr) spawn glue.
//!
//! The agent runs in-process `flanneld` (the Rust port, link_repos/
//! flannel-rust) with `kube_subnet_mgr: true`, pointed at the same
//! apiserver URL the kubelet uses. flanneld leases this node's podCIDR
//! (the `/24` the server-side node-cidr controller wrote), writes
//! `subnet.env`, programs the vxlan/host-gw data plane and patches the
//! Node `NetworkUnavailable` condition via `PATCH /nodes/{name}/status`.
//! The flannel CNI meta-plugin itself is the `flannel` multicall alias
//! (Q1): containerd exec's it from the conflist (staged by runtime::stage).
//!
//! C8 (long-run bug sweep): the daemon task is a supervised restart loop,
//! so a flanneld exit is never terminal - it comes back under backoff and
//! the node's `NetworkUnavailable` condition tracks it both ways (see
//! [`crate::flannel_keeper`]).
//!
//! Backend selection comes from net-conf.json (upstream semantics), which
//! this module synthesizes from `--cluster-cidr` + `--flannel-backend`
//! (`vxlan` default; `none` disables the daemon entirely, k3s parity).
//! A `--flannel-conf` file is passed through verbatim instead.
#![forbid(unsafe_code)]

use std::io;
use std::path::{Path, PathBuf};

use tokio::task::JoinHandle;

/// Agent flannel flags captured from clap (T4.3 Scope A).
#[derive(Debug, Clone, Default)]
pub struct FlannelNet {
    /// `--cluster-cidr`: the pod network (net-conf `Network`).
    pub cluster_cidr: Option<String>,
    /// `--flannel-backend`: `vxlan` (default) | `host-gw` | `none`.
    pub backend: Option<String>,
    /// `--flannel-iface`: interface flanneld binds the data plane on.
    pub iface: Option<String>,
    /// `--flannel-conf`: verbatim net-conf.json passthrough.
    pub conf: Option<PathBuf>,
}

/// `--flannel-backend=none` keeps the agent flannel-free (k3s parity).
pub fn is_enabled(net: &FlannelNet) -> bool {
    net.backend.as_deref() != Some("none")
}

/// Normalized backend type for net-conf.json. Unknown values degrade to
/// `vxlan` with a warning (k3s errors; we prefer booting over dying —
/// the same degraded-start convention as the kubelet path).
pub fn normalize_backend(net: &FlannelNet) -> (&'static str, Option<String>) {
    match net.backend.as_deref() {
        None | Some("") | Some("vxlan") => ("vxlan", None),
        Some("host-gw") => ("host-gw", None),
        Some(other) => ("vxlan", Some(other.to_string())),
    }
}

/// `<data-dir>/agent/etc/flannel/net-conf.json` (flanneld reads it; the
/// kube subnet manager parses it before leasing).
pub fn net_conf_path(data_dir: &Path) -> PathBuf {
    data_dir.join("agent/etc/flannel/net-conf.json")
}

/// `<data-dir>/agent/flannel/subnet.env` (flanneld writes it; the flannel
/// CNI meta-plugin reads it through `FLANNEL_SUBNET_FILE` — the Rust CNI
/// NetConf has no `subnetFile` JSON key, so containerd's env carries it).
pub fn subnet_env_path(data_dir: &Path) -> PathBuf {
    data_dir.join("agent/flannel/subnet.env")
}

/// `<data-dir>/agent/flannel/kubeconfig.yaml` (T1.3a, Q47 P3): the
/// token-carrying kubeconfig handed to the embedded flanneld. Only
/// written when the agent runs with `--token`; absent = anonymous v1.
pub fn kubeconfig_path(data_dir: &Path) -> PathBuf {
    data_dir.join("agent/flannel/kubeconfig.yaml")
}

/// Render the minimal kubeconfig flannel-rust's `from_kubeconfig` accepts:
/// one context/cluster/user, `token` auth ONLY (client certs error there).
/// The explicit `--kube-api-url` still overrides the server field, so the
/// token is the only thing this file actually contributes.
pub fn kubeconfig_body(api_url: &str, token: &str) -> String {
    format!(
        "apiVersion: v1\nkind: Config\ncurrent-context: init-pro\nclusters:\n- \
         name: init-pro\n  cluster:\n    server: {api_url}\ncontexts:\n- \
         name: init-pro\n  context:\n    cluster: init-pro\n    user: init-pro\nusers:\n- \
         name: init-pro\n  user:\n    token: {token}\n"
    )
}

/// Write the flanneld kubeconfig when a token is present (idempotent,
/// content-compared like [`ensure_net_conf`]). Tokens containing YAML/
/// header-hostile bytes (whitespace, quotes, control chars) are rejected
/// so the file can never parse into an unintended credential.
pub fn ensure_kubeconfig(data_dir: &Path, api_url: &str, token: &str) -> io::Result<PathBuf> {
    if token.is_empty()
        || token
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || c == '"' || c == '\'')
    {
        return Err(io::Error::other(
            "flannel kubeconfig: token contains unsupported characters",
        ));
    }
    let path = kubeconfig_path(data_dir);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let body = kubeconfig_body(api_url, token);
    let needs_write = match std::fs::read_to_string(&path) {
        Ok(existing) => existing != body,
        Err(_) => true,
    };
    if needs_write {
        std::fs::write(&path, body)?;
    }
    Ok(path)
}

/// net-conf.json body synthesized from the wired flags (k3s defaults:
/// vxlan backend, no IPv6). Backend.Type drives flanneld's data plane.
pub fn net_conf_body(net: &FlannelNet) -> String {
    let (backend, _) = normalize_backend(net);
    let network = net
        .cluster_cidr
        .clone()
        .unwrap_or_else(|| "10.42.0.0/16".to_string());
    serde_json::json!({
        // Key order is the wire-visible body (serde_json preserve_order is
        // unified on workspace-wide by the rdb work): keep Backend first to
        // match upstream flannel's documented net-conf shape.
        "Backend": { "Type": backend },
        "Network": network,
    })
    .to_string()
}

/// Materialize net-conf.json (idempotent: an identical body is not
/// rewritten, so flanneld restarts never churn the file mtime).
/// `--flannel-conf FILE` wins: its path is returned verbatim.
pub fn ensure_net_conf(data_dir: &Path, net: &FlannelNet) -> io::Result<PathBuf> {
    if let Some(user) = &net.conf {
        return Ok(user.clone());
    }
    let path = net_conf_path(data_dir);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let body = net_conf_body(net);
    let needs_write = match std::fs::read_to_string(&path) {
        Ok(existing) => existing != body,
        Err(_) => true,
    };
    if needs_write {
        std::fs::write(&path, body)?;
    }
    Ok(path)
}

/// C8: a flanneld run shorter than this is part of a crash loop; a run
/// that stayed up this long resets the restart streak (and the backoff).
const STABLE_RUN: std::time::Duration = std::time::Duration::from_secs(30);

/// C8: consecutive-restart streak after a run that lasted `ran_for`.
/// `prev + 1` inside a crash loop, `0` once the daemon proved stable, so
/// the first retry after a healthy run is back at the base delay.
pub(crate) fn next_restart_streak(prev: u32, ran_for: std::time::Duration) -> u32 {
    if ran_for >= STABLE_RUN {
        0
    } else {
        prev.saturating_add(1)
    }
}

/// C8: delay before the next flanneld attempt - 250 ms doubling on the
/// consecutive-restart streak, capped at 5 s. Reuses the pure
/// `backoff_delay` the containerd supervisor already uses, so both
/// planes back off alike (no second backoff implementation to drift).
pub(crate) fn restart_delay(restarts: u32) -> std::time::Duration {
    runtime::backoff_delay(
        restarts,
        std::time::Duration::from_millis(250),
        std::time::Duration::from_secs(5),
    )
}

/// Spawn flanneld as a supervised task. Sets `NODE_NAME` first (process
/// env: the kube subnet manager resolves the node BEFORE its first API
/// call), bridges the shared [`Shutdown`] onto a `CancellationToken`
/// (flanneld has no infra::Shutdown seam), and never installs its own
/// signal handlers (`install_signal_handlers: false` — this process owns
/// SIGINT/SIGTERM). C8: the returned handle drives a supervised restart
/// loop, not one `flanneld::run` (see the loop body).
pub fn spawn_flannel(
    data_dir: &Path,
    api_url: &str,
    node_name: &str,
    net: &FlannelNet,
    // T1.3a (Q47 P3): bearer token for the kube subnet-mgr API calls —
    // injected through a generated kubeconfig (flannel-rust reads
    // `user.token`, never env tokens); `None` keeps anonymous v1.
    token: Option<&str>,
    shutdown: infra::Shutdown,
) -> io::Result<JoinHandle<()>> {
    let net_conf = ensure_net_conf(data_dir, net)?;
    let subnet_file = subnet_env_path(data_dir);
    if let Some(parent) = subnet_file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let (backend, rejected) = normalize_backend(net);
    if let Some(bad) = rejected {
        tracing::warn!(
            target: "init-pro",
            backend = %bad,
            "unknown --flannel-backend; falling back to vxlan"
        );
    }
    // Process-global, single write, before any flanneld code runs (the
    // subnet manager reads NODE_NAME on startup, Q34).
    std::env::set_var("NODE_NAME", node_name);

    // T1.3a (Q47 P3): with a token, flanneld reads server+credentials
    // from the generated kubeconfig (the explicit kube_api_url still
    // wins for the server); without one, anonymous plain HTTP (Q21).
    let kubeconfig_file = match token {
        Some(t) => match ensure_kubeconfig(data_dir, api_url, t) {
            Ok(p) => p.to_string_lossy().into_owned(),
            Err(e) => {
                tracing::warn!(
                    target: "init-pro",
                    error = %e,
                    "flannel: token kubeconfig unavailable; running unauthenticated"
                );
                String::new()
            }
        },
        None => String::new(),
    };
    let opts = flanneld::Options {
        kube_subnet_mgr: true,
        kube_api_url: api_url.to_string(),
        // Upstream Go default (flannel.alpha.coreos.com): the prefix is
        // regex-validated, an empty string would abort SubnetManager setup.
        kube_annotation_prefix: "flannel.alpha.coreos.com".to_string(),
        kubeconfig_file,
        iface: net.iface.iter().cloned().collect(),
        iface_regex: Vec::new(),
        // k3s parity: masquerade pod -> off-node traffic by default.
        ip_masq: true,
        // flanneld validates the margin to (0, 24h): 0 is REJECTED ("out of
        // acceptable range"), so pin Go's default 10 minutes explicitly.
        subnet_lease_renew_margin: 10,
        // `Options::default()` zero-initializes both flags; the Go/flag
        // defaults are `--iptables-resync=5` and `--iptables-forward-rules
        // =true`. The forward ACCEPT rules are load-bearing: dockerd leaves
        // the FORWARD chain at policy DROP (its own ACCEPTs are all
        // `-i/-o docker0`-scoped), so without them same-node pod->pod
        // traffic is silently dropped under `bridge-nf-call-iptables=1`.
        iptables_resync_seconds: 5,
        iptables_forward_rules: true,
        subnet_file: subnet_file.to_string_lossy().into_owned(),
        net_config_path: net_conf.to_string_lossy().into_owned(),
        // Health endpoint off under the embedder (the agent's own logs
        // + the Node heartbeat are the liveness surface).
        healthz_port: 0,
        set_node_network_unavailable: true,
        // This process owns SIGINT/SIGTERM via infra::signal.
        install_signal_handlers: false,
        ..flanneld::Options::default()
    };
    tracing::info!(
        target: "init-pro",
        node = %node_name,
        api = %api_url,
        backend,
        net_conf = %net_conf.display(),
        subnet_env = %subnet_file.display(),
        "starting embedded flanneld (kube subnet-mgr, T4.3/Q34)"
    );
    let alive = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let alive_for_daemon = alive.clone();
    let api = api_url.to_string();
    let node = node_name.to_string();
    let daemon_token = token.map(str::to_string);
    let daemon_shutdown = shutdown.clone();
    let daemon = tokio::spawn(async move {
        // C8 (long-run bug sweep): a flanneld exit is NOT terminal. One
        // `run` used to be the whole task, so a crashed daemon left the
        // node without pod networking until agent restart while the Node
        // object still advertised `NetworkUnavailable=False` (the scheduler
        // kept placing pods there). The loop below restarts it in-place
        // under one JoinHandle, so the caller's drain is unchanged; each
        // attempt gets a fresh cancel token bridged onto the global
        // shutdown, and the keeper flips the Node condition back once the
        // daemon is up again.
        let mut restarts: u32 = 0;
        loop {
            let cancel = tokio_util::sync::CancellationToken::new();
            let bridge = cancel.clone();
            let shutdown_waiter = daemon_shutdown.clone();
            let bridger = tokio::spawn(async move {
                shutdown_waiter.cancelled().await;
                bridge.cancel();
            });
            alive_for_daemon.store(true, std::sync::atomic::Ordering::Relaxed);
            let started = std::time::Instant::now();
            let outcome = flanneld::run(opts.clone(), cancel).await;
            bridger.abort();
            match outcome {
                Ok(code) => {
                    tracing::info!(target: "init-pro", code, "flanneld exited");
                }
                Err(e) => {
                    tracing::warn!(
                        target: "init-pro",
                        error = %e,
                        "flanneld failed (networking degraded)"
                    );
                }
            }
            alive_for_daemon.store(false, std::sync::atomic::Ordering::Relaxed);
            if daemon_shutdown.is_triggered() {
                break;
            }
            // C8: best-effort immediate network-down patch, so the
            // scheduler stops placing pods before the keeper's next beat.
            crate::flannel_keeper::mark_network_down(&api, &node, daemon_token.as_deref()).await;
            restarts = next_restart_streak(restarts, started.elapsed());
            let delay = restart_delay(restarts);
            tracing::warn!(
                target: "init-pro",
                node = %node,
                restarts = restarts + 1,
                backoff_ms = delay.as_millis() as u64,
                "flanneld exited; restarting under supervision (C8)"
            );
            // C8: every retry sleep is bound to the shutdown token.
            tokio::select! {
                _ = daemon_shutdown.cancelled() => break,
                _ = tokio::time::sleep(delay) => {}
            }
        }
    });
    crate::flannel_keeper::spawn_condition_keeper(api_url, node_name, token, alive, shutdown);
    Ok(daemon)
}

#[cfg(test)]
#[path = "flannel_tests.rs"]
mod tests;
