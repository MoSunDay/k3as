//! Subcommand implementations (Phase 1).
//!
//! `server` installs the graceful-shutdown handler, builds the served schema
//! registry, and serves the T1.2a HTTP discovery endpoints (`/api`, `/apis`,
//! `/api/v1`, `/apis/<g>/<v>`) on the bind address — draining on signal.
//! `agent` installs the handler and idles until Layers 3–4 land. `stage`
//! exposes the T0.2 manifest contract + B5 runtime staging.

use std::net::SocketAddr;
use std::process::ExitCode;

use init_pro_core::embed::EmbeddedManifest;
use init_pro_infra::{Config, Shutdown};

/// Server-only wiring: where to bind the discovery API + whether it is disabled.
pub struct ServerBind {
    pub addr: SocketAddr,
    pub disable_apiserver: bool,
}

pub fn run_server(cfg: Config, bind: ServerBind) -> ExitCode {
    run_supervised("server", cfg, Some(bind))
}

pub fn run_agent(cfg: Config) -> ExitCode {
    run_supervised("agent", cfg, None)
}

/// `server` serves discovery when given a [`ServerBind`]; `agent` only idles
/// until Layers 3–4 land.
fn run_supervised(role: &'static str, cfg: Config, bind: Option<ServerBind>) -> ExitCode {
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name(role)
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("init-pro {role}: failed to start runtime: {e}");
            return ExitCode::FAILURE;
        }
    };

    let result = rt.block_on(async move {
        let shutdown = Shutdown::new();

        if let Err(e) = init_pro_infra::signal::install(shutdown.clone()).await {
            tracing::warn!(target: "init-pro", role, "signal handler install failed: {e}");
        }

        // T1.2a: build the served schema registry, serve HTTP discovery, and
        // keep a handle so the process waits for full drain on shutdown.
        let server_join = match &bind {
            Some(b) if !b.disable_apiserver => {
                let reg = crate::discovery::served_schema();
                let advertised = b.addr.to_string();
                let summary = crate::discovery::served_groups_summary(&reg, &advertised);
                tracing::info!(target: "init-pro", role, "T1.1 {summary}");
                let server_shutdown = shutdown.clone();
                let addr = b.addr;
                Some(tokio::spawn(async move {
                    if let Err(e) =
                        init_pro_apiserver::serve(reg, addr, advertised, async move {
                            server_shutdown.cancelled().await;
                        })
                        .await
                    {
                        tracing::error!(target: "init-pro", role, "apiserver exited: {e}");
                    }
                }))
            }
            Some(_) => {
                tracing::info!(target: "init-pro", role, "apiserver disabled by flag");
                None
            }
            None => None,
        };

        tracing::info!(
            target: "init-pro",
            role,
            version = init_pro_core::version(),
            data_dir = %cfg.data_dir.display(),
            "init-pro {role} ready",
        );

        shutdown.cancelled().await;
        tracing::info!(target: "init-pro", role, "init-pro {role}: draining");
        if let Some(jh) = server_join {
            let _ = jh.await;
        }
        tracing::info!(target: "init-pro", role, "init-pro {role}: draining complete");
        Ok::<(), std::io::Error>(())
    });

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("init-pro {role}: {e}");
            ExitCode::FAILURE
        }
    }
}

/// `stage --dry-run` lists the embedded manifest + hashes without writing.
/// `stage` (no dry-run) performs the live atomic staging (B5).
pub fn run_stage(cfg: Config, dry_run: bool, manifest: &EmbeddedManifest) -> ExitCode {
    if dry_run {
        print_dry_run(&cfg, manifest);
        return ExitCode::SUCCESS;
    }

    match crate::stage::stage(&cfg, manifest.assets, manifest.sha256_sums) {
        Ok(result) => {
            if result.staged {
                println!(
                    "init-pro stage: staged {} asset(s) -> data/{}",
                    manifest.assets.len(),
                    result.hash
                );
            } else {
                println!("init-pro stage: up-to-date (data/current -> {})", result.hash);
            }
            println!("data/current: {}", result.current.display());
            for entry in &result.path_entries {
                println!("PATH+: {}", entry.display());
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("init-pro stage: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Print the dry-run manifest listing.
fn print_dry_run(cfg: &Config, manifest: &EmbeddedManifest) {
    let assets = manifest.assets;
    println!("init-pro stage --dry-run");
    println!("data-dir: {}", cfg.data_dir.display());
    println!(
        "manifest ({} embedded asset{}):",
        assets.len(),
        if assets.len() == 1 { "" } else { "s" }
    );
    if assets.is_empty() {
        println!("  (none — build with INIT_PRO_EMBED=1 to bake vendor artifacts)");
    } else {
        let total = assets.iter().map(|a| a.size).sum::<u64>();
        for a in assets {
            println!("  {:<28} {} bytes  sha256={}", a.path, a.size, a.sha256);
        }
        println!("total uncompressed: {} bytes", total);
    }
    let sums_lines = manifest.sha256_sums.lines().count();
    let links_lines = manifest.data_links.lines().count();
    println!(".sha256sums: {} entries", sums_lines);
    println!(".links: {} entries", links_lines);
}
