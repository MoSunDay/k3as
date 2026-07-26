//! T0.2 acquire driver (Q6). Reads `vendor/versions.toml` and stages pinned
//! upstream artifacts into `vendor/bin/` via `init-pro-vendor`.
//!
//! Default (`cargo build`) is **Auto**: use the cache if present, else skip
//! with a warning (no network) — so the dev loop and `cargo test` stay fast
//! and network-free. Set `INIT_PRO_VENDOR=1` to download missing artifacts;
//! `INIT_PRO_OFFLINE=1` forbids network and requires a pre-populated cache.
//!
//! B1 owns acquire only; the embed/`assets.rs` codegen lands in B2.

#![forbid(unsafe_code)]

use std::path::PathBuf;

fn main() {
    // Repo root = <this crate>/../..  (crates/init-pro -> crates -> repo).
    let crate_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let repo_root = crate_dir
        .parent()
        .and_then(|p| p.parent())
        .expect("CARGO_MANIFEST_DIR is nested under <repo>/crates/<crate>");
    let manifest_path = repo_root.join("vendor").join("versions.toml");
    let vendor_root = repo_root.join("vendor");

    println!("cargo:rerun-if-changed={}", manifest_path.display());
    println!("cargo:rerun-if-env-changed=INIT_PRO_VENDOR");
    println!("cargo:rerun-if-env-changed=INIT_PRO_OFFLINE");

    let src = match std::fs::read_to_string(&manifest_path) {
        Ok(s) => s,
        Err(e) => {
            // No manifest yet (e.g. vendoring not in use): nothing to do.
            println!(
                "cargo:warning=vendor: no manifest at {} ({}); skipping acquire",
                manifest_path.display(),
                e
            );
            return;
        }
    };

    let artifacts = match init_pro_vendor::parse(&src) {
        Ok(a) => a,
        Err(e) => {
            println!("cargo:warning=vendor: manifest invalid: {e}");
            std::process::exit(1);
        }
    };

    let mode = init_pro_vendor::mode_from_env();
    match init_pro_vendor::run(&vendor_root, &artifacts, mode) {
        Ok(rep) => {
            if rep.skipped > 0 {
                println!(
                    "cargo:warning=vendor: {} (skipped — set INIT_PRO_VENDOR=1 to acquire)",
                    rep.summary()
                );
            } else {
                println!("cargo:warning=vendor: {}", rep.summary());
            }
        }
        Err(e) => {
            let hint = e.hint();
            if hint.is_empty() {
                println!("cargo:warning=vendor: acquire failed: {e}");
            } else {
                println!("cargo:warning=vendor: acquire failed: {e}\n  hint: {hint}");
            }
            std::process::exit(1);
        }
    }
}
