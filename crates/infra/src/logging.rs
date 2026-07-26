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

#[cfg(test)]
mod tests {
    use super::*;

    /// Serialize RUST_LOG-touching tests (env is process-global).
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn init_does_not_panic_with_debug_false() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var("RUST_LOG");
        // Observable contract: never panics for either debug flag.
        init(false);
    }

    #[test]
    fn init_does_not_panic_with_debug_true() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var("RUST_LOG");
        init(true);
    }

    #[test]
    fn init_is_idempotent() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var("RUST_LOG");
        // First call installs the global subscriber; later calls must be
        // swallowed (try_init error ignored) rather than panic.
        init(false);
        init(true);
        init(false);
    }

    #[test]
    fn init_with_rust_log_set_does_not_panic() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var("RUST_LOG", "debug");
        // Exercises EnvFilter::try_from_default_env's Ok path.
        init(false);
        std::env::remove_var("RUST_LOG");
    }
}
