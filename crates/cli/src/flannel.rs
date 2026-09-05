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

/// Spawn flanneld as a supervised task. Sets `NODE_NAME` first (process
/// env: the kube subnet manager resolves the node BEFORE its first API
/// call), bridges the shared [`Shutdown`] onto a `CancellationToken`
/// (flanneld has no infra::Shutdown seam), and never installs its own
/// signal handlers (`install_signal_handlers: false` — this process owns
/// SIGINT/SIGTERM).
pub fn spawn_flannel(
    data_dir: &Path,
    api_url: &str,
    node_name: &str,
    net: &FlannelNet,
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

    let opts = flanneld::Options {
        kube_subnet_mgr: true,
        // Plain HTTP, no auth in v1 (Q21) — flannel's kube client is
        // unauthenticated by design here; kubeconfig_file stays empty.
        kube_api_url: api_url.to_string(),
        // Upstream Go default (flannel.alpha.coreos.com): the prefix is
        // regex-validated, an empty string would abort SubnetManager setup.
        kube_annotation_prefix: "flannel.alpha.coreos.com".to_string(),
        kubeconfig_file: String::new(),
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
    let cancel = tokio_util::sync::CancellationToken::new();
    let bridge = cancel.clone();
    tokio::spawn(async move {
        shutdown.cancelled().await;
        bridge.cancel();
    });
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
    let daemon = tokio::spawn(async move {
        match flanneld::run(opts, cancel).await {
            Ok(code) => {
                tracing::info!(target: "init-pro", code, "flanneld exited");
            }
            Err(e) => {
                tracing::warn!(target: "init-pro", error = %e, "flanneld failed (networking degraded)");
            }
        }
        alive_for_daemon.store(false, std::sync::atomic::Ordering::Relaxed);
    });
    crate::flannel_keeper::spawn_condition_keeper(api_url, node_name, alive);
    Ok(daemon)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("init-pro-flannel-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn backend_normalization_and_disabled() {
        assert!(is_enabled(&FlannelNet::default()));
        assert!(!is_enabled(&FlannelNet {
            backend: Some("none".into()),
            ..Default::default()
        }));
        assert_eq!(normalize_backend(&FlannelNet::default()), ("vxlan", None));
        assert_eq!(
            normalize_backend(&FlannelNet {
                backend: Some("host-gw".into()),
                ..Default::default()
            }),
            ("host-gw", None)
        );
        let (_, warn) = normalize_backend(&FlannelNet {
            backend: Some("wireguard".into()),
            ..Default::default()
        });
        assert_eq!(warn.as_deref(), Some("wireguard"));
    }

    #[test]
    fn net_conf_body_default_and_custom() {
        let body = net_conf_body(&FlannelNet::default());
        assert_eq!(
            body,
            r#"{"Backend":{"Type":"vxlan"},"Network":"10.42.0.0/16"}"#
        );
        let body = net_conf_body(&FlannelNet {
            cluster_cidr: Some("10.244.0.0/16".into()),
            backend: Some("host-gw".into()),
            ..Default::default()
        });
        assert_eq!(
            body,
            r#"{"Backend":{"Type":"host-gw"},"Network":"10.244.0.0/16"}"#
        );
    }

    #[test]
    fn paths_follow_agent_layout() {
        let dd = Path::new("/tmp/dd");
        assert_eq!(
            net_conf_path(dd),
            Path::new("/tmp/dd/agent/etc/flannel/net-conf.json")
        );
        assert_eq!(
            subnet_env_path(dd),
            Path::new("/tmp/dd/agent/flannel/subnet.env")
        );
    }

    #[test]
    fn ensure_net_conf_writes_idempotently_and_honors_user_file() {
        let dd = tmp_dir("netconf");
        let net = FlannelNet {
            cluster_cidr: Some("10.43.0.0/16".into()),
            ..Default::default()
        };
        let p = ensure_net_conf(&dd, &net).unwrap();
        assert_eq!(p, net_conf_path(&dd));
        let first = std::fs::read_to_string(&p).unwrap();
        assert_eq!(first, net_conf_body(&net));
        // Second call: no rewrite churn (content identical).
        let mtime1 = std::fs::metadata(&p).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        ensure_net_conf(&dd, &net).unwrap();
        let mtime2 = std::fs::metadata(&p).unwrap().modified().unwrap();
        assert_eq!(mtime1, mtime2, "identical body must not rewrite");

        // Changed flags DO rewrite.
        let net2 = FlannelNet {
            backend: Some("host-gw".into()),
            ..net
        };
        ensure_net_conf(&dd, &net2).unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), net_conf_body(&net2));

        // --flannel-conf passes a user file through verbatim (no write).
        let user = dd.join("user-net-conf.json");
        std::fs::write(&user, r#"{"Network":"192.168.0.0/16"}"#).unwrap();
        let net3 = FlannelNet {
            conf: Some(user.clone()),
            ..Default::default()
        };
        assert_eq!(ensure_net_conf(&dd, &net3).unwrap(), user);

        let _ = std::fs::remove_dir_all(&dd);
    }
}
