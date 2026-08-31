#![forbid(unsafe_code)]

//! Shared domain primitives used by every layer. Kept dependency-free so heavy
//! layers can depend on it without pulling the world.

pub mod embed;
pub mod time;
pub mod workspace;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub fn version() -> &'static str {
    VERSION
}

pub use embed::EmbeddedAsset;
pub use workspace::{
    agent_workspace_path, workflow_workspace_path, workspace_nfs_url, MOUNT_TARGET,
    WORKSPACE_ANNOTATION,
};
