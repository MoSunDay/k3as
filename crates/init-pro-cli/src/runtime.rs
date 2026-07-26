//! Subcommand implementations (Phase 1 stubs).
//!
//! `server` / `agent` install the graceful-shutdown handler and then idle until
//! a signal arrives — enough to exercise the infra (T0.3) and to host real
//! layers as they land. `stage` exposes the T0.2 manifest contract.

use std::process::ExitCode;

use init_pro_core::embed::EmbeddedAsset;
use init_pro_infra::{Config, Shutdown};

pub fn run_server(cfg: Config) -> ExitCode {
    run_supervised("server", cfg)
}

pub fn run_agent(cfg: Config) -> ExitCode {
    run_supervised("agent", cfg)
}

/// `server` / `agent` differ only in role name until Layers 1–4 land.
fn run_supervised(role: &'static str, cfg: Config) -> ExitCode {
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

        tracing::info!(
            target: "init-pro",
            role,
            version = init_pro_core::version(),
            data_dir = %cfg.data_dir.display(),
            "init-pro {role} ready (Phase 1 stub; Layers 1–4 arrive in Phase 2)",
        );

        shutdown.cancelled().await;
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
/// `stage` (no dry-run) is the live atomic-staging path arriving in B5.
pub fn run_stage(cfg: Config, dry_run: bool, embedded: &[EmbeddedAsset]) -> ExitCode {
    if dry_run {
        println!("init-pro stage --dry-run");
        println!("data-dir: {}", cfg.data_dir.display());
        println!("manifest ({} embedded asset{}):", embedded.len(), if embedded.len() == 1 { "" } else { "s" });
        if embedded.is_empty() {
            println!("  (none — build with INIT_PRO_EMBED=1 to bake vendor artifacts)");
        } else {
            let total = embedded.iter().map(|a| a.size).sum::<u64>();
            for a in embedded {
                println!(
                    "  {:<28} {} bytes  sha256={}",
                    a.path, a.size, a.sha256
                );
            }
            println!("total uncompressed: {} bytes", total);
        }
        ExitCode::SUCCESS
    } else {
        eprintln!("stage: live staging arrives in B5 (use --dry-run to inspect the manifest)");
        ExitCode::FAILURE
    }
}
