//! Generated asset registry — spliced from `build.rs` output (T0.2 B2/B3).
//!
//! `build.rs` always writes `$OUT_DIR/assets.rs` (full when `INIT_PRO_EMBED=1`,
//! empty otherwise). We include it here so `main.rs` and `runtime.rs` can
//! access:
//! - [`embedded_assets`] — the `&[EmbeddedAsset]` slice
//! - [`SHA256_SUMS`] — the `.sha256sums` manifest (k3s dataverify parity)
//! - [`DATA_LINKS`] — the `.links` manifest

#![forbid(unsafe_code)]

include!(concat!(env!("OUT_DIR"), "/assets.rs"));
