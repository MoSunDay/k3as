//! clap-derive command definitions for the k3s-compatible surface (Q9 matrix,
//! TODO **T0.4**). Only the **accept-wired** flags (Table A) get clap fields;
//! the ~113 accept-no-op-warn flags are handled by a pre-clap strip filter
//! (A3), keeping these structs small.

pub mod agent;
pub mod server;

pub use agent::AgentCmd;
pub use server::ServerCmd;

use std::path::PathBuf;

/// S+A wired flags shared by `server` and `agent` (Table A, scope `S+A`).
///
/// Flattened into both commands so the flag vocabulary is declared once.
#[derive(Debug, Clone, clap::Args)]
pub struct WiredShared {
    /// Config file (pre-clap pre-scan, Q8). env `INIT_PRO_CONFIG_FILE`.
    #[arg(short = 'c', long = "config", value_name = "FILE", env = "INIT_PRO_CONFIG_FILE")]
    pub config: Option<PathBuf>,

    /// Cluster join secret. env `INIT_PRO_TOKEN`.
    #[arg(short = 't', long = "token", env = "INIT_PRO_TOKEN")]
    pub token: Option<String>,

    /// Server URL to join. env `INIT_PRO_URL`.
    #[arg(short = 's', long = "server", env = "INIT_PRO_URL")]
    pub server: Option<String>,

    /// Flip child PATH ordering: `bin/aux` ahead of host PATH (Q6 stage).
    #[arg(long = "prefer-bundled-bin")]
    pub prefer_bundled_bin: bool,
}
