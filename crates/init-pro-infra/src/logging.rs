//! `tracing` initialization with k3s `--debug` parity.
//!
//! First call wins (subscriber is global). `RUST_LOG` always overrides, like
//! the rest of the Rust ecosystem expects.

/// Install the global subscriber. `debug=true` lowers the default floor to DEBUG
/// (mirrors k3s `--debug`), unless `RUST_LOG` already says otherwise.
pub fn init(debug: bool) {
    use tracing_subscriber::{fmt, EnvFilter};

    let default = if debug { "debug" } else { "info" };
    let filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new(default))
        .unwrap_or_else(|_| EnvFilter::new("info"));

    // Ignore the error if a subscriber is already installed (e.g. in tests).
    let _ = fmt().with_env_filter(filter).with_target(false).try_init();
}
