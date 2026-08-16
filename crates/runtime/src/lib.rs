//! Node runtime bundle (TODO **T4.1**, decision **Q25**).
//!
//! Production wiring of the Q24-spiked chain: Rust-side containerd config
//! templating ([`config`]), idempotent vendor staging ([`stage`]), and a
//! supervising runner with socket health, exponential backoff and drain
//! ([`supervisor`]). [`start_agent_runtime`] composes the three for the
//! `init-pro agent` path (Sprint 16). The CRI driver landed with T4.2
//! (decision **Q26** route B): [`cri_json`] models the crictl JSON wire
//! shapes and [`cri`] drives the vendored crictl as a subprocess against
//! the supervisor's socket.

#![forbid(unsafe_code)]

pub mod config;
pub mod cri;
pub mod cri_json;
pub mod stage;
pub mod supervisor;

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use infra::Shutdown;
use tokio::sync::watch;

pub use config::{render, ContainerdConfigVars, DEFAULT_SANDBOX_IMAGE};
pub use cri::{staged_crictl, CriCtl, CriError};
pub use cri_json::{ContainerConfig, PodSandboxConfig};
pub use stage::{stage_containerd_tree, vendor_bin_root, StageOutcome};
pub use supervisor::{
    backoff_delay, supervise, supervisor_args, wait_socket, SuperviseStats, SupervisorSpec,
};

/// k3s-style agent runtime paths under one data dir (single source for the
/// bootstrap, the supervisor, and the multicall peer re-exec).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRuntimePaths {
    /// `<dd>/agent/containerd` — the staged bundle + shims + runc + aux.
    pub runtime_dir: PathBuf,
    /// `<dd>/agent/etc/containerd/config.toml`.
    pub config_path: PathBuf,
    /// `<dd>/run/containerd/containerd.sock`.
    pub socket: PathBuf,
}

impl AgentRuntimePaths {
    pub fn for_data_dir(data_dir: &Path) -> Self {
        Self {
            runtime_dir: data_dir.join("agent").join("containerd"),
            config_path: data_dir
                .join("agent")
                .join("etc")
                .join("containerd")
                .join("config.toml"),
            socket: data_dir
                .join("run")
                .join("containerd")
                .join("containerd.sock"),
        }
    }
}

/// Bootstrap the agent's containerd: stage the vendor bundle (or reuse an
/// already-staged tree), render the config + CNI conf, and spawn the
/// supervisor task. The caller drains the returned handle on shutdown.
///
/// Errors when neither a vendor bundle nor a staged tree is available —
/// the agent then logs + degrades (v1 behavior; T4.2 hardens the
/// requirement once the kubelet equivalent consumes the CRI socket).
pub fn start_agent_runtime(
    data_dir: &Path,
    shutdown: Shutdown,
) -> io::Result<tokio::task::JoinHandle<SuperviseStats>> {
    let paths = AgentRuntimePaths::for_data_dir(data_dir);
    let vars = ContainerdConfigVars::for_data_dir(data_dir, &config::sandbox_image());

    match stage::vendor_bin_root() {
        Some(vendor) => {
            let out = stage::stage_containerd_tree(&vendor, &paths.runtime_dir)?;
            if out.is_noop() {
                tracing::info!(
                    target: "init-pro",
                    dir = %paths.runtime_dir.display(),
                    "containerd tree already staged (no-op)"
                );
            } else {
                tracing::info!(
                    target: "init-pro",
                    copied = ?out.copied,
                    skipped = out.skipped.len(),
                    missing = ?out.missing,
                    "staged containerd tree"
                );
            }
        }
        None if paths.runtime_dir.join("containerd").is_file() => {
            tracing::info!(
                target: "init-pro",
                dir = %paths.runtime_dir.display(),
                "no vendor bundle; reusing staged containerd tree"
            );
        }
        None => {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "no containerd source: run 'INIT_PRO_VENDOR=1 cargo build' or set \
                 INIT_PRO_VENDOR_BIN, or pre-populate <data-dir>/agent/containerd",
            ));
        }
    }

    if let Some(parent) = paths.config_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&paths.config_path, render(&vars))?;
    stage::write_cni_conf(&vars.cni_conf_dir)?;

    // A stale socket from an unclean shutdown would both confuse the health
    // probe and block containerd's bind; the daemon is down by assumption here.
    let _ = fs::remove_file(&paths.socket);

    let mut spec = SupervisorSpec::new(
        paths.runtime_dir.join("containerd"),
        &paths.config_path,
        &paths.socket,
    );
    spec.path_prefix = Some(paths.runtime_dir.clone());

    let (pid_tx, _pid_rx) = watch::channel::<Option<u32>>(None);
    tracing::info!(
        target: "init-pro",
        socket = %paths.socket.display(),
        sandbox_image = %vars.sandbox_image,
        "containerd supervisor starting"
    );
    Ok(tokio::spawn(async move {
        supervise(spec, shutdown, pid_tx).await
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_runtime_paths_match_config_vars_layout() {
        // The config template and the supervisor must agree on the socket:
        // the templated [grpc] address IS the health-probe target.
        let dd = Path::new("/dd");
        let paths = AgentRuntimePaths::for_data_dir(dd);
        let vars = ContainerdConfigVars::for_data_dir(dd, DEFAULT_SANDBOX_IMAGE);
        assert_eq!(paths.socket, vars.socket);
        assert_eq!(paths.runtime_dir, vars.root.parent().unwrap());
        assert_eq!(
            paths.config_path,
            dd.join("agent/etc/containerd/config.toml")
        );
    }
}
