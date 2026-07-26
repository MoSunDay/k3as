//! `init-pro server` — k3s-compatible accept-wired flags (Table A, scope `S`).

use crate::cmd::WiredShared;

/// `init-pro server` wired flags (accept-wired subset, Q9).
///
/// Phase 1 captures these into a typed struct; honoring them lands with the
/// owning layers (etcd T2.x, datastore T2.x, component controllers T6.x).
#[derive(Debug, Clone, clap::Args)]
pub struct ServerCmd {
    #[command(flatten)]
    pub shared: WiredShared,

    /// Disable packaged components (validated against `DisableItems` in A4).
    #[arg(long = "disable", value_delimiter = ',')]
    pub disable: Vec<String>,

    /// Disable embedded etcd (etcd runtime = T2.1).
    #[arg(long = "disable-etcd")]
    pub disable_etcd: bool,

    /// Disable the apiserver.
    #[arg(long = "disable-apiserver")]
    pub disable_apiserver: bool,

    /// Disable the local kubelet/agent run.
    #[arg(long = "disable-agent")]
    pub disable_agent: bool,

    /// Disable kube-controller-manager.
    #[arg(long = "disable-controller-manager")]
    pub disable_controller_manager: bool,

    /// Disable kube-scheduler.
    #[arg(long = "disable-scheduler")]
    pub disable_scheduler: bool,

    /// Disable the k3s cloud-controller-manager.
    #[arg(long = "disable-cloud-controller")]
    pub disable_cloud_controller: bool,

    /// Disable kube-proxy.
    #[arg(long = "disable-kube-proxy")]
    pub disable_kube_proxy: bool,

    /// Disable the network-policy controller.
    #[arg(long = "disable-network-policy")]
    pub disable_network_policy: bool,

    /// Disable the helm controller.
    #[arg(long = "disable-helm-controller")]
    pub disable_helm_controller: bool,

    /// External datastore DSN (storage backend wiring = T2.x).
    /// env `INIT_PRO_DATASTORE_ENDPOINT`.
    #[arg(long = "datastore-endpoint", env = "INIT_PRO_DATASTORE_ENDPOINT")]
    pub datastore_endpoint: Option<String>,

    /// Bootstrap a new cluster (etcd init = T2.1/T3.4).
    /// env `INIT_PRO_CLUSTER_INIT`.
    #[arg(long = "cluster-init", env = "INIT_PRO_CLUSTER_INIT")]
    pub cluster_init: bool,
}
