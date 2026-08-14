#![forbid(unsafe_code)]

//! Shared domain primitives used by every layer. Kept dependency-free so heavy
//! layers can depend on it without pulling the world.

pub mod embed;
pub mod time;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub fn version() -> &'static str {
    VERSION
}

pub use embed::EmbeddedAsset;
