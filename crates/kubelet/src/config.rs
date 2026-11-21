//! Kubelet configuration (TODO **T4.2**; T1.3a token passthrough, Q47).
//!
//! Extracted from `runner` so the loop module stays under the file-size
//! budget: [`KubeletConfig`] carries every knob the `agent` CLI wiring
//! overrides, with k3s-like defaults from [`KubeletConfig::new`].

use std::path::PathBuf;
use std::time::Duration;

/// Configuration for [`spawn`]. All knobs are public so callers (the
/// `agent` CLI wiring) can override; [`KubeletConfig::new`] gives k3s-like
/// defaults (2s sync, 10s heartbeat, env-driven sandbox image).
#[derive(Debug, Clone)]
pub struct KubeletConfig {
    pub server_url: String,
    pub node_name: String,
    pub data_dir: PathBuf,
    pub sandbox_image: String,
    pub sync_period: Duration,
    pub heartbeat_period: Duration,
    /// IP the kubelet's local log/exec listener binds AND that it advertises
    /// on the Node object (`status.addresses[InternalIP]`) so the apiserver
    /// relay can reach it (T4.2 Scope C, Q31). Default `127.0.0.1`.
    pub node_ip: String,
    /// Port of the same listener (upstream kubelet convention: 10250);
    /// published via the Node annotation `init-pro.io/kubelet-port`.
    pub kubelet_port: u16,
    /// `status.nodeInfo.containerRuntimeVersion`. Default mirrors the
    /// staged containerd (`containerd://1.7.20`); the built-in engine
    /// (**T4.12**, Q49) overrides via [`Self::with_runtime_version`] with
    /// `engine://<version>` so `kubectl describe node` tells the truth.
    pub runtime_version: String,
    /// True when the kubelet runs the BUILT-IN engine backend (T4.12) —
    /// the GPU/host-direct guard input (T7.2, Q61: no containerd fallback).
    pub engine_builtin: bool,
    /// Rootfs mode the agent was started with (Q60): the GPU plane is
    /// eligible only under the literal host rootfs mode.
    pub builtin_rootfs: crate::engine::RootfsMode,
    /// `--gpu-inventory` source (T7.2, Q61): `auto` (detect, default),
    /// `none`, or `fake:<resource>:<count>` for fake-scope evidence.
    pub gpu_inventory: crate::gpu::GpuInventorySource,
    /// Static bearer token for the apiserver (T1.3a, **Q47** P3):
    /// `--token`/`INIT_PRO_TOKEN` passthrough — `None` keeps the v1
    /// default of anonymous plain-HTTP access.
    pub server_token: Option<String>,
}

impl KubeletConfig {
    /// Defaults: sandbox image from `INIT_PRO_SANDBOX_IMAGE` (else the
    /// pinned [`runtime::DEFAULT_SANDBOX_IMAGE`]), 2s sync, 10s heartbeat.
    pub fn new(
        server_url: impl Into<String>,
        node_name: impl Into<String>,
        data_dir: impl Into<PathBuf>,
    ) -> Self {
        Self {
            server_url: server_url.into(),
            node_name: node_name.into(),
            data_dir: data_dir.into(),
            sandbox_image: runtime::config::sandbox_image(),
            sync_period: Duration::from_secs(2),
            heartbeat_period: Duration::from_secs(10),
            node_ip: "127.0.0.1".to_string(),
            kubelet_port: 10250,
            runtime_version: "containerd://1.7.20".to_string(),
            // T7.2/Q61 defaults: containerd backend, non-GPU node, auto inventory.
            engine_builtin: false,
            builtin_rootfs: crate::engine::RootfsMode::Host,
            gpu_inventory: crate::gpu::GpuInventorySource::Auto,
            server_token: None,
        }
    }

    /// Override the advertised/bind node IP for the local log+exec listener.
    pub fn with_node_ip(mut self, ip: impl Into<String>) -> Self {
        self.node_ip = ip.into();
        self
    }

    /// Override the local log+exec listener port (default 10250).
    pub fn with_kubelet_port(mut self, port: u16) -> Self {
        self.kubelet_port = port;
        self
    }

    /// Override `nodeInfo.containerRuntimeVersion` (**T4.12**: the
    /// built-in engine reports `engine://<common::version()>`).
    pub fn with_runtime_version(mut self, v: impl Into<String>) -> Self {
        self.runtime_version = v.into();
        self
    }

    /// Attach the apiserver bearer token (T1.3a, Q47 P3).
    pub fn with_server_token(mut self, token: Option<String>) -> Self {
        self.server_token = token;
        self
    }

    /// Per-pod log root handed to the CRI sandbox configs.
    pub fn log_root(&self) -> PathBuf {
        self.data_dir.join("pods")
    }
}
