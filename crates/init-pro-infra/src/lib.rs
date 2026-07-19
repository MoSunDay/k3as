//! Shared infrastructure for every layer (TODO **T0.3**).
//!
//! - [`logging`] — `tracing` init with k3s `--debug` parity
//! - [`config`] — layered config (CLI flag > env > file > default), `--data-dir` aware
//! - [`signal`] — graceful shutdown coordination on SIGTERM/SIGINT
#![forbid(unsafe_code)]

pub mod config;
pub mod logging;
pub mod signal;

pub use config::Config;
pub use signal::Shutdown;
