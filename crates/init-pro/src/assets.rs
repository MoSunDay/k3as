//! Generated asset registry — spliced from `build.rs` output (T0.2 B2).
//!
//! `build.rs` always writes `$OUT_DIR/assets.rs` (full when `INIT_PRO_EMBED=1`,
//! empty otherwise). We include it here and re-export the function so `main.rs`
//! can pass the `&'static` slice into the CLI driver.

#![forbid(unsafe_code)]

include!(concat!(env!("OUT_DIR"), "/assets.rs"));
