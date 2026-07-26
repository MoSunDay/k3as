//! Build-time embedded asset descriptor + manifest bundle (T0.2, Q6).
//!
//! One per-file zstd-compressed blob baked into the binary at compile time.
//! The generated `assets.rs` (produced by `init_pro_vendor::embed::generate`)
//! constructs `&'static [EmbeddedAsset]` via `include_bytes!`, plus the
//! `.sha256sums` and `.links` manifest strings (B3). Runtime `stage()`
//! (B5) consumes this to decompress + verify + write into the data dir.

#![forbid(unsafe_code)]

/// One embedded artifact (content-addressed by SHA-256 of the original bytes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EmbeddedAsset {
    /// Logical path under the data dir, e.g. `"bin/runc"`, `"bin/aux/bridge"`.
    pub path: &'static str,
    /// Hex SHA-256 of the **original uncompressed** bytes (the integrity pin).
    pub sha256: &'static str,
    /// Original (uncompressed) size in bytes.
    pub size: u64,
    /// zstd-compressed bytes (level 19 target, Q6).
    pub zstd: &'static [u8],
}

/// Bundle of all build-time embedded data passed to the CLI/runtime layer.
///
/// Constructed in `init-pro::assets` from the generated `assets.rs`.
#[derive(Debug, Clone, Copy)]
pub struct EmbeddedManifest {
    /// The per-file asset registry.
    pub assets: &'static [EmbeddedAsset],
    /// `.sha256sums` content (k3s dataverify parity, B3).
    pub sha256_sums: &'static str,
    /// `.links` content (symlink directives, B3).
    pub data_links: &'static str,
}
