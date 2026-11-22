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
    /// container log/exec listener on (k3s `--node-ip` parity; T4.2 Scope
    /// C, Q31 — the apiserver relay dials this address). DEFAULT (T4.5,
    /// Q65): unset -> auto-detect the primary outbound IP towards the
    /// apiserver host (bind-less UDP connect; fallback `127.0.0.1`) so the
    /// direct relay AND the reverse tunnel both reach the kubelet.
    #[arg(long = "node-ip", value_name = "IP")]
    pub node_ip: Option<String>,

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

    /// Default registry the built-in engine pulls images from when the
    /// image reference carries no host of its own (T4.13 M2, Q54 — init-pro
    /// extension, not k3s parity; mirrors the server's registry flag).
    /// env `INIT_PRO_REGISTRY_ADDR`.
    #[arg(
        long = "registry-addr",
        env = "INIT_PRO_REGISTRY_ADDR",
        default_value = registry::DEFAULT_ADDR,
        value_name = "ADDR"
    )]
    pub registry_addr: String,

    /// Soft quota cap (BYTES) on the built-in engine's image store
    /// (T4.13 M5, Q53 — init-pro extension): checked BEFORE a pull
    /// downloads blobs/layers; a pull that would exceed the cap fails
    /// cleanly (the pod stays Pending, the agent survives). 0 disables
    /// the cap entirely (the DEFAULT). env `INIT_PRO_BUILTIN_IMAGE_QUOTA`.
    #[arg(
        long = "builtin-image-quota",
        env = "INIT_PRO_BUILTIN_IMAGE_QUOTA",
        default_value_t = 0,
        value_name = "BYTES"
    )]
    pub builtin_image_quota: u64,

    /// Rootfs source for the built-in engine (T4.14, Q55 — init-pro
    /// extension): `host` (DEFAULT) treats the image reference as a host
    /// directory (an absolute path, or a name staged under
    /// `<data_dir>/engine/images/<name>`); `image` keeps the T4.13
    /// registry-pull chain. Per-pod override: the `init-pro.io/rootfs`
    /// annotation. env `INIT_PRO_BUILTIN_ROOTFS`.
    #[arg(
        long = "builtin-rootfs",
        env = "INIT_PRO_BUILTIN_ROOTFS",
        default_value = "host",
        value_name = "MODE",
        value_parser = ["host", "image"]
    )]
    pub builtin_rootfs: String,

    /// Whole-GPU inventory source (T7.2, Q61 — init-pro extension):
    /// `auto` (DEFAULT) probes nvidia-smi / rocm-smi / sysfs once per
    /// heartbeat; `none` turns the GPU plane off; `fake:<resource>:<count>`
    /// publishes a fixed device count for fake-scope evidence (e.g.
    /// `fake:nvidia.com/gpu:2`). env `INIT_PRO_GPU_INVENTORY`.
    #[arg(
        long = "gpu-inventory",
        env = "INIT_PRO_GPU_INVENTORY",
        default_value = "auto",
        value_name = "SOURCE"
    )]
    pub gpu_inventory: String,

    /// Node-level NetworkPolicy enforcement (TODO **T4.4**, decision
    /// **Q64** — k3s parity): default ON; the node-side keeper compiles
    /// `networking.k8s.io/v1` NetworkPolicies into iptables (flannel
    /// data plane). `--disable-network-policy` opts THIS node out of
    /// enforcement (policy objects stay accepted/stored server-side).
    /// env `INIT_PRO_DISABLE_NETWORK_POLICY`.
    #[arg(
        long = "disable-network-policy",
        env = "INIT_PRO_DISABLE_NETWORK_POLICY",
        default_value_t = false
    )]
    pub disable_network_policy: bool,

    /// Node-level MANDATORY Agent hardening (T7.7, Q63 — init-pro
    /// extension): Agents WITHOUT `spec.sandbox` stay Pending
    /// (`SandboxRequired`) instead of running the legacy direct Q37
    /// subprocess; implies INIT_PRO_AGENT_SANDBOX enablement.
    /// env `INIT_PRO_AGENT_REQUIRE_SANDBOX`: clap v4 reads the env value
    /// through the bool value parser, so ONLY the exact spellings
    /// `true`/`false` are accepted here (the node-side
    /// `SandboxNodeConfig::from_env` reader additionally accepts
    /// `1`/`yes`/`on`).
    #[arg(
        long = "agent-require-sandbox",
        env = "INIT_PRO_AGENT_REQUIRE_SANDBOX",
        default_value_t = false
    )]
    pub agent_require_sandbox: bool,

    /// Disable the agent-dialed reverse relay tunnel (TODO **T4.5**,
    /// decision **Q65** — init-pro extension; k3s has no equivalent): the
    /// node then serves kubelet API hops (logs/exec) ONLY over the direct
    /// apiserver->node dial. env `INIT_PRO_DISABLE_PROXY_TUNNEL`.
    #[arg(
        long = "disable-proxy-tunnel",
        env = "INIT_PRO_DISABLE_PROXY_TUNNEL",
        default_value_t = false
    )]
    pub disable_proxy_tunnel: bool,
}
