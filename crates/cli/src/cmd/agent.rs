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

    /// IPv4 pod network: flanneld leases this node's `/24` from the Node
    /// `spec.podCIDR` allocated under it and synthesizes net-conf.json
    /// `Network` from it (T4.3 Scope A, Q34). env `INIT_PRO_CLUSTER_CIDR`.
    #[arg(
        long = "cluster-cidr",
        env = "INIT_PRO_CLUSTER_CIDR",
        default_value = "10.42.0.0/16",
        value_name = "CIDR"
    )]
    pub cluster_cidr: String,

    /// flannel data-plane backend: `vxlan` (default) | `host-gw` |
    /// `none` (disables the embedded flanneld; k3s parity).
    /// env `INIT_PRO_FLANNEL_BACKEND`.
    #[arg(
        long = "flannel-backend",
        env = "INIT_PRO_FLANNEL_BACKEND",
        default_value = "vxlan",
        value_name = "BACKEND"
    )]
    pub flannel_backend: String,

    /// Disable the node-side Agent execution loop (T7.1 Phase B, Q37):
    /// bound Agents stay `Pending` (only the scheduler's `Scheduled`
    /// condition is written). env `INIT_PRO_DISABLE_AGENT_EXEC` (clap bool
    /// env: ONLY the spellings `true`/`false` are accepted).
    #[arg(long = "disable-agent-exec", env = "INIT_PRO_DISABLE_AGENT_EXEC")]
    pub disable_agent_exec: bool,

    /// anyhome NFS endpoint the Agent runner mounts workspaces from when an
    /// Agent carries no `init-pro.io/workspace` annotation (T7.1 Phase B,
    /// Q36/Q37). Defaults to the server's anyhome default, so single-node
    /// setups need no flag; multi-node agents point at the server's
    /// `--anyhome-addr`. env `INIT_PRO_ANYHOME_ADDR`.
    #[arg(
        long = "anyhome-addr",
        env = "INIT_PRO_ANYHOME_ADDR",
        default_value = "127.0.0.1:11111",
        value_name = "ADDR"
    )]
    pub anyhome_addr: String,

    /// Interface (name or IP) flanneld binds the data plane on; defaults
    /// to autodetection (k3s `--flannel-iface` parity).
    /// env `INIT_PRO_FLANNEL_IFACE`.
    #[arg(
        long = "flannel-iface",
        env = "INIT_PRO_FLANNEL_IFACE",
        value_name = "IFACE"
    )]
    pub flannel_iface: Option<String>,

    /// Verbatim net-conf.json passthrough — skips the synthesized
    /// `agent/etc/flannel/net-conf.json` (k3s `--flannel-conf` parity).
    /// env `INIT_PRO_FLANNEL_CONF`.
    #[arg(
        long = "flannel-conf",
        env = "INIT_PRO_FLANNEL_CONF",
        value_name = "FILE"
    )]
    pub flannel_conf: Option<std::path::PathBuf>,
}
