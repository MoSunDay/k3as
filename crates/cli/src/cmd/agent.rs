//! `init-pro agent` — k3s-compatible accept-wired flags (Table A, scope `A`).
//!
//! The agent's wired subset is the shared S+A flags plus `--node-name`
//! (wired with the T4.2 kubelet); every other k3s agent flag is
//! accept-no-op-warn (Table C), handled by the pre-clap strip filter (A3).

use crate::cmd::WiredShared;

/// `init-pro agent` wired flags (accept-wired subset, Q9).
#[derive(Debug, Clone, clap::Args)]
pub struct AgentCmd {
    #[command(flatten)]
    pub shared: WiredShared,

    /// Node name to register (k3s `--node-name` parity); defaults to the hostname.
    #[arg(long = "node-name")]
    pub node_name: Option<String>,

    /// IP the kubelet advertises on the Node object and binds its local
    /// container log/exec listener on (k3s `--node-ip` parity; T4.2 Scope C,
    /// Q31 — the apiserver relay dials this address).
    #[arg(long = "node-ip", default_value = "127.0.0.1", value_name = "IP")]
    pub node_ip: String,

    /// Port of the kubelet's local log/exec listener, advertised via the
    /// Node annotation `init-pro.io/kubelet-port` (T4.2 Scope C, Q31).
    /// env `INIT_PRO_KUBELET_PORT`.
    #[arg(
        long = "kubelet-port",
        env = "INIT_PRO_KUBELET_PORT",
        default_value_t = 10250,
        value_name = "PORT"
    )]
    pub kubelet_port: u16,
}
