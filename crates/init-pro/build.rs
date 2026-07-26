//! T0.2 acquire + embed + SBOM driver (Q6/Q7). Reads `vendor/versions.toml`,
//! stages pinned upstream artifacts into `vendor/bin/` (B1), runs the Q7
//! license allow-list gate + writes SPDX-2.3 SBOM to `LICENSES/` (B4), and —
//! when `INIT_PRO_EMBED=1` — zstd-compresses each file and generates
//! `$OUT_DIR/assets.rs` (B2) for `include_bytes!` embedding.
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
    let artifacts = match vendor::parse(&src) {
        Ok(a) => a,
        Err(e) => {
            println!("cargo:warning=vendor: manifest invalid: {e}");
            std::process::exit(1);
        }
    };

    // --- B4: license gate + SPDX SBOM ------------------------------------
    if let Err(e) = vendor::sbom::validate(&artifacts) {
        println!("cargo:warning=vendor: license gate FAILED: {e}");
        std::process::exit(1);
    }
    let spdx = vendor::sbom::render_spdx(&artifacts, &iso_now());
    let licenses_dir = repo_root.join("LICENSES");
    let spdx_path = licenses_dir.join("spdx-2.3.json");
    if let Err(e) = std::fs::create_dir_all(&licenses_dir).and_then(|_| std::fs::write(&spdx_path, &spdx)) {
        println!("cargo:warning=vendor: failed to write SPDX SBOM to {}: {e}", spdx_path.display());
    }
    println!("cargo:warning=vendor: license gate passed (Q7); SPDX SBOM -> LICENSES/spdx-2.3.json");

    // --- B1 (cont): acquire ----------------------------------------------
    let mode = vendor::mode_from_env();
    match vendor::run(&vendor_root, &artifacts, mode) {
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
        match vendor::generate(vendor_bin, &out_dir, 19) {
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
        vendor::generate_empty(&out_dir).expect("write empty assets.rs");
    }
}

fn write_empty_assets() {
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR set by cargo"));
    vendor::generate_empty(&out_dir).expect("write empty assets.rs");
}

/// Current UTC date as ISO-8601 (for SPDX `created`).
fn iso_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (y, m, d) = epoch_to_ymd(secs as i64 / 86400);
    format!("{y:04}-{m:02}-{d:02}T00:00:00Z")
}

/// Days since 1970-01-01 → (year, month, day). Howard Hinnant's algorithm.
fn epoch_to_ymd(days: i64) -> (i64, u32, u32) {
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}
