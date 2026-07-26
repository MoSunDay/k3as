//! The `init-pro` CLI surface (TODO **T0.4** lays out full k3s parity; this is
//! the Phase 1 spine that routes the top-level + forced subcommands).
//!
//! Two entry points:
//! - [`run`] — drive from the real `std::env::args()`.
//! - [`run_forced`] — used by the multicall dispatcher when a symlinked alias
//!   (`server`, `agent`, ...) selects a known init-pro subcommand.

mod config_scan;
mod runtime;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{CommandFactory, Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "init-pro",
    bin_name = "init-pro",
    version = init_pro_core::version(),
    about = "A single-binary, k3s-compatible Kubernetes distribution with a built-in Lua Router",
    long_about = None,
    propagate_version = true,
)]
pub struct Cli {
    /// Data directory (k3s `-d` / `--data-dir` parity).
    #[arg(short = 'd', long = "data-dir", global = true, value_name = "DIR")]
    pub data_dir: Option<PathBuf>,

    /// Turn on debug logging (k3s `--debug` parity).
    #[arg(long = "debug", global = true)]
    pub debug: bool,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Run the control-plane server.
    Server,
    /// Run the node agent.
    Agent,
    /// Stage bundled peer binaries to the data dir.
    Stage {
        /// List the embedded manifest + hashes without writing anything.
        #[arg(long = "dry-run")]
        dry_run: bool,
    },
}

/// Drive the CLI from the real process args.
pub fn run() -> ExitCode {
    run_from(std::env::args())
}

/// Drive the CLI as if the given subcommand were selected (multicall forced).
pub fn run_forced(subcmd: &str, extra: &[String]) -> ExitCode {
    let prog = std::env::args()
        .next()
        .unwrap_or_else(|| "init-pro".to_string());
    let argv = std::iter::once(prog)
        .chain(std::iter::once(subcmd.to_string()))
        .chain(extra.iter().cloned());
    run_from(argv)
}

fn run_from<I, S>(argv: I) -> ExitCode
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    // Collect into Vec<String> so clap can consume it.
    let argv: Vec<String> = argv.into_iter().map(Into::into).collect();

    // Pre-clap config-file pre-scan (Q8): surface --config/-c without clap.
    let cli_config = config_scan::pre_scan_config(&argv);

    let cli = match Cli::try_parse_from(argv) {
        Ok(c) => c,
        Err(e) => {
            // `--help` / `--version` come through here and exit with the right code.
            e.exit();
        }
    };

    init_pro_infra::logging::init(cli.debug);
    let cfg = init_pro_infra::Config::resolve(
        cli.data_dir.as_deref(),
        Some(cli.debug),
        cli_config.as_deref(),
    );
    tracing::debug!(target: "init-pro", data_dir = ?cfg.data_dir, is_debug = cfg.debug, "config resolved");

    match cli.command {
        Some(Command::Server) => runtime::run_server(cfg),
        Some(Command::Agent) => runtime::run_agent(cfg),
        Some(Command::Stage { dry_run }) => runtime::run_stage(cfg, dry_run),
        None => {
            // `init-pro` with no subcommand: print help to stdout, exit 0
            // (k3s prints usage; we keep it success so the contract is friendly).
            let _ = Cli::command().print_long_help();
            println!();
            ExitCode::SUCCESS
        }
    }
}
