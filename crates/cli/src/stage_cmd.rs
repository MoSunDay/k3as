//! `stage` subcommand face (split from `runtime.rs`, T4.13 cleanup):
//! dry-run manifest listing + live atomic staging (B5).

use std::process::ExitCode;

use common::embed::EmbeddedManifest;
use infra::Config;

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
                println!(
                    "init-pro stage: up-to-date (data/current -> {})",
                    result.hash
                );
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
