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
}
