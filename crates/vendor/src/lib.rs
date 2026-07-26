//! `vendor` — pinned upstream-artifact acquire (T0.2, Q6).
//!
//! Reads `vendor/versions.toml`, downloads each pinned artifact, verifies its
//! SHA-256, and stages it into `vendor/bin/`. The `build.rs` of the `init-pro`
//! bin crate drives this; `INIT_PRO_OFFLINE=1` / `INIT_PRO_VENDOR=1` select the
//! air-gap / explicit-acquire modes (see [`acquire::Mode`]).
//!
//! Subprocess bundling only (containerd / runc / CNI multicall); etcd embed/FFI
//! is T2.1's concern (Q6). License allow-list (Q7) is enforced at parse time.

#![forbid(unsafe_code)]

pub mod acquire;
pub mod dataverify;
pub mod digest;
pub mod embed;
pub mod manifest;
pub mod sbom;

pub use acquire::{mode_from_env, run, AcquireError, Action, Mode, Report};
pub use embed::{generate, generate_empty, EmbedError, EmbedReport};
pub use manifest::{parse, Artifact, Kind, ParseError};
