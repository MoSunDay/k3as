//! T0.2 acquire + embed driver (Q6). Reads `vendor/versions.toml`, stages
//! pinned upstream artifacts into `vendor/bin/` (B1), and — when
//! `INIT_PRO_EMBED=1` — zstd-compresses each file and generates `$OUT_DIR/
//! assets.rs` (B2) for `include_bytes!` embedding.
//!
//! Default (`cargo build`) is **Auto** acquire (cache-or-skip, no network) +
//! empty embed registry — so the dev loop and `cargo test` stay fast. Set
//! `INIT_PRO_VENDOR=1` to download missing artifacts; `INIT_PRO_OFFLINE=1`
//! forbids network; `INIT_PRO_EMBED=1` to bake blobs into the binary.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};

fn main() {
    let crate_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let repo_root = crate_dir
        .parent()
        .and_then(|p| p.parent())
        .expect("CARGO_MANIFEST_DIR is nested under <repo>/crates/<crate>");
    let manifest_path = repo_root.join("vendor").join("versions.toml");
    let vendor_root = repo_root.join("vendor");
    let vendor_bin = vendor_root.join("bin");

    println!("cargo:rerun-if-changed={}", manifest_path.display());
    println!("cargo:rerun-if-env-changed=INIT_PRO_VENDOR");
    println!("cargo:rerun-if-env-changed=INIT_PRO_OFFLINE");
    println!("cargo:rerun-if-env-changed=INIT_PRO_EMBED");

    // --- B1: acquire ------------------------------------------------------
    let src = match std::fs::read_to_string(&manifest_path) {
        Ok(s) => s,
        Err(e) => {
            println!("cargo:warning=vendor: no manifest at {} ({})", manifest_path.display(), e);
            write_empty_assets();
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

    // --- B2: embed codegen -------------------------------------------------
    write_assets(&vendor_bin);
}

fn env_truthy(key: &str) -> bool {
    matches!(
        std::env::var(key),
        Ok(v) if v == "1" || v.eq_ignore_ascii_case("true")
    )
}

/// Generate `assets.rs`: full embed (INIT_PRO_EMBED=1) or empty (default).
fn write_assets(vendor_bin: &Path) {
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR set by cargo"));
    if env_truthy("INIT_PRO_EMBED") {
        match init_pro_vendor::generate(vendor_bin, &out_dir, 19) {
            Ok(rep) => {
                println!("cargo:warning=vendor: embed: {}", rep.summary());
                for f in &rep.files {
                    println!("cargo:rerun-if-changed={}", f.display());
                }
            }
            Err(e) => {
                println!("cargo:warning=vendor: embed failed: {e}");
                std::process::exit(1);
            }
        }
    } else {
        init_pro_vendor::generate_empty(&out_dir).expect("write empty assets.rs");
    }
}

fn write_empty_assets() {
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR set by cargo"));
    init_pro_vendor::generate_empty(&out_dir).expect("write empty assets.rs");
}
