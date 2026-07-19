//! init-pro-core: shared domain primitives (placeholder).
//!
//! Fills out with API model / resource types as Layers 1+ land. Kept dependency
//! free so every layer can depend on it without pulling the world.
#![forbid(unsafe_code)]

/// Crate version (= the single binary's version).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Build identifier string used in logs and `--version`.
pub fn version() -> &'static str {
    VERSION
}
