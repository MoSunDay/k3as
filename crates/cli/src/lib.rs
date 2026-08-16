//! The `init-pro` CLI surface (TODO **T0.4** lays out full k3s parity; this is
//! the Phase 1 spine that routes the top-level + forced subcommands).
//!
//! Two entry points:
//! - [`run`] — drive from the real `std::env::args()`.
//! - [`run_forced`] — used by the multicall dispatcher when a symlinked alias
//!   (`server`, `agent`, ...) selects a known init-pro subcommand.

mod cmd;
mod config_scan;
pub mod discovery;
mod flags;
mod runtime;
mod stage;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{CommandFactory, Parser, Subcommand};
use common::embed::EmbeddedManifest;

#[derive(Parser, Debug)]
#[command(
    name = "init-pro",
    bin_name = "init-pro",
    version = common::version(),
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
    Server(cmd::ServerCmd),
    /// Run the node agent.
    Agent(cmd::AgentCmd),
    /// Stage bundled peer binaries to the data dir.
    Stage {
        /// List the embedded manifest + hashes without writing anything.
        #[arg(long = "dry-run")]
        dry_run: bool,
    },
}

/// Drive the CLI from the real process args.
pub fn run(manifest: &EmbeddedManifest) -> ExitCode {
    run_from(std::env::args(), manifest)
}

/// Drive the CLI as if the given subcommand were selected (multicall forced).
pub fn run_forced(subcmd: &str, extra: &[String], manifest: &EmbeddedManifest) -> ExitCode {
    let prog = std::env::args()
        .next()
        .unwrap_or_else(|| "init-pro".to_string());
    let argv = std::iter::once(prog)
        .chain(std::iter::once(subcmd.to_string()))
        .chain(extra.iter().cloned());
    run_from(argv, manifest)
}

fn run_from<I, S>(argv: I, manifest: &EmbeddedManifest) -> ExitCode
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    // Collect into Vec<String> so clap can consume it.
    let argv: Vec<String> = argv.into_iter().map(Into::into).collect();

    // Pre-clap strip of accept-no-op-warn flags (Q9, A3): removes the ~108
    // no-op flags (and their values) so clap only sees the 17 wired flags.
    let strip = flags::strip_noop(&argv);

    // Pre-clap config-file pre-scan (Q8): surface --config/-c without clap.
    let cli_config = config_scan::pre_scan_config(&strip.argv);

    let cli = match Cli::try_parse_from(strip.argv) {
        Ok(c) => c,
        Err(e) => {
            // `--help` / `--version` come through here and exit with the right code.
            e.exit();
        }
    };

    // Fatal conflict validation (Table B, A4): exit non-zero with the
    // k3s-parity message BEFORE logging/resolve so a fatal is clean.
    match &cli.command {
        Some(Command::Server(svc)) => {
            if let Err(flags::conflicts::Fatal(msg)) =
                flags::conflicts::validate_server(svc, &strip.seen)
            {
                eprintln!("{msg}");
                return ExitCode::FAILURE;
            }
        }
        Some(Command::Agent(ag)) => {
            if let Err(flags::conflicts::Fatal(msg)) = flags::conflicts::validate_agent(ag) {
                eprintln!("{msg}");
                return ExitCode::FAILURE;
            }
        }
        _ => {}
    }

    infra::logging::init(cli.debug);
    flags::warn_noops(&strip.seen);
    let cfg = infra::Config::resolve(
        cli.data_dir.as_deref(),
        Some(cli.debug),
        cli_config.as_deref(),
    );
    tracing::debug!(target: "init-pro", data_dir = ?cfg.data_dir, is_debug = cfg.debug, "config resolved");

    match cli.command {
        Some(Command::Server(svc)) => {
            tracing::debug!(target: "init-pro", "server flags captured: {:?}", svc);
            match format!("{}:{}", svc.bind_address, svc.https_listen_port)
                .parse::<std::net::SocketAddr>()
            {
                Ok(addr) => runtime::run_server(
                    cfg,
                    runtime::ServerBind {
                        addr,
                        disable_apiserver: svc.disable_apiserver,
                        disable_controllers: svc.disable_controller_manager,
                        disable_scheduler: svc.disable_scheduler,
                        disable_kube_proxy: svc.disable_kube_proxy,
                        scheduler_args: svc.kube_scheduler_arg.clone(),
                    },
                ),
                Err(e) => {
                    eprintln!(
                        "init-pro server: invalid --bind-address/--https-listen-port ({}:{}): {e}",
                        svc.bind_address, svc.https_listen_port
                    );
                    ExitCode::FAILURE
                }
            }
        }
        Some(Command::Agent(ag)) => {
            tracing::debug!(target: "init-pro", "agent flags captured: {:?}", ag);
            runtime::run_agent(cfg, ag.shared.server.clone(), ag.node_name.clone())
        }
        Some(Command::Stage { dry_run }) => runtime::run_stage(cfg, dry_run, manifest),
        None => {
            // `init-pro` with no subcommand: print help to stdout, exit 0
            // (k3s prints usage; we keep it success so the contract is friendly).
            let _ = Cli::command().print_long_help();
            println!();
            ExitCode::SUCCESS
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    /// Global wired flags appear at the top level; subcommand-scoped wired
    /// flags appear in the subcommand help (Table A scope correctness).
    #[test]
    fn wired_flags_appear_in_help_surface() {
        let mut cmd = Cli::command();

        // Globals (--data-dir/--debug) are top-level options.
        let root_help = cmd.render_help().to_string();
        for flag in ["--data-dir", "--debug"] {
            assert!(root_help.contains(flag), "root help missing global {flag}");
        }

        // Server-scoped + shared wired flags appear in `server --help`.
        let mut server = cmd
            .find_subcommand("server")
            .expect("server subcommand exists")
            .clone();
        let server_help = server.render_help().to_string();
        for flag in [
            "--config",
            "--disable",
            "--disable-etcd",
            "--disable-apiserver",
            "--disable-agent",
            "--disable-controller-manager",
            "--disable-scheduler",
            "--disable-cloud-controller",
            "--disable-kube-proxy",
            "--disable-network-policy",
            "--disable-helm-controller",
            "--datastore-endpoint",
            "--prefer-bundled-bin",
            "--token",
            "--server",
            "--cluster-init",
        ] {
            assert!(
                server_help.contains(flag),
                "server --help missing wired flag {flag}"
            );
        }

        // Shared (S+A) wired flags appear in `agent --help`.
        let mut agent = cmd
            .find_subcommand("agent")
            .expect("agent subcommand exists")
            .clone();
        let agent_help = agent.render_help().to_string();
        for flag in [
            "--config",
            "--token",
            "--server",
            "--prefer-bundled-bin",
            "--node-name",
        ] {
            assert!(
                agent_help.contains(flag),
                "agent --help missing wired flag {flag}"
            );
        }
    }

    /// Server-only flags must NOT appear in `agent --help` (scope correctness).
    #[test]
    fn agent_help_excludes_server_only_flags() {
        let cmd = Cli::command();
        let mut agent = cmd.find_subcommand("agent").expect("agent exists").clone();
        let help = agent.render_help().to_string();
        for flag in [
            "--cluster-init",
            "--datastore-endpoint",
            "--disable-network-policy",
        ] {
            assert!(
                !help.contains(flag),
                "agent --help should not list server-only flag {flag}"
            );
        }
    }

    /// A type-correct wired flag must be accepted (no "unknown" error).
    #[test]
    fn server_accepts_wired_flags_without_error() {
        let argv = [
            "init-pro",
            "server",
            "--data-dir",
            "/tmp/dd",
            "--disable",
            "coredns",
            "--datastore-endpoint",
            "mysql://x",
            "--cluster-init",
            "--token",
            "secret",
        ];
        let cli = Cli::try_parse_from(argv).expect("wired flags accepted");
        match cli.command {
            Some(Command::Server(s)) => {
                assert_eq!(s.disable, vec!["coredns".to_string()]);
                assert!(s.cluster_init);
                assert_eq!(s.shared.token.as_deref(), Some("secret"));
            }
            _ => panic!("expected Server"),
        }
    }
}
