//! Layered runtime config: CLI flag > env > config file > default.
//!
//! k3s resolves `--data-dir`/`--debug` early (before full flag parse) via a
//! config-file pre-scan; init-pro mirrors that with [`Config::resolve`].

use std::path::{Path, PathBuf};

/// Default data dir (k3s default is `/var/lib/rancher/k3s`; we use our own root).
pub const DEFAULT_DATA_DIR: &str = "/var/lib/init-pro";

/// Resolved runtime configuration honoring k3s precedence.
#[derive(Debug, Clone)]
pub struct Config {
    /// k3s `-d`/`--data-dir`.
    pub data_dir: PathBuf,
    /// k3s `--debug` parity.
    pub debug: bool,
}

/// Which layer a value came from (for tracing / tests).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layer {
    Default,
    File,
    Env,
    Cli,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            data_dir: PathBuf::from(DEFAULT_DATA_DIR),
            debug: false,
        }
    }
}

impl Config {
    /// Resolve the config from defaults + file + env + CLI overrides.
    ///
    /// `cli_data_dir` / `cli_debug` are `Some` only when the caller actually
    /// received the flag (so absence falls through to lower layers).
    pub fn resolve(cli_data_dir: Option<&Path>, cli_debug: Option<bool>) -> Self {
        let mut cfg = Config::default();

        // File layer: a real config-file pre-scan lands with T0.4 (clap + serde).
        if let Some(dd) = file_layer_data_dir() {
            cfg.data_dir = dd;
        }
        if let Some(d) = file_layer_debug() {
            cfg.debug = d;
        }

        // Env layer (INIT_PRO_* mirrors k3s env conventions).
        if let Ok(v) = std::env::var("INIT_PRO_DATA_DIR") {
            if !v.trim().is_empty() {
                cfg.data_dir = PathBuf::from(v);
            }
        }
        if let Ok(v) = std::env::var("INIT_PRO_DEBUG") {
            cfg.debug = parse_bool(&v);
        }

        // CLI layer wins.
        if let Some(d) = cli_data_dir {
            cfg.data_dir = d.to_path_buf();
        }
        if let Some(d) = cli_debug {
            cfg.debug = d;
        }

        cfg
    }

    /// `<data-dir>/bin` — where bundled peers get staged (k3s `bin/` parity).
    pub fn bin_dir(&self) -> PathBuf {
        self.data_dir.join("bin")
    }

    /// Where the layer says a given value came from (best-effort, for tracing).
    pub fn data_dir_layer(&self, cli_data_dir: Option<&Path>) -> Layer {
        if cli_data_dir.is_some() {
            Layer::Cli
        } else if std::env::var_os("INIT_PRO_DATA_DIR").is_some() {
            Layer::Env
        } else if file_layer_data_dir().is_some() {
            Layer::File
        } else {
            Layer::Default
        }
    }
}

fn parse_bool(v: &str) -> bool {
    matches!(
        v.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

// Placeholders for the config-file pre-scan (filled by T0.4). Always None today
// so the default + env + CLI layers fully determine the result.
fn file_layer_data_dir() -> Option<PathBuf> {
    None
}
fn file_layer_debug() -> Option<bool> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serialize the env-touching tests so they don't race under cargo's
    /// parallel test runner.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn default_data_dir() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var("INIT_PRO_DATA_DIR");
        std::env::remove_var("INIT_PRO_DEBUG");
        let c = Config::resolve(None, None);
        assert_eq!(c.data_dir, PathBuf::from(DEFAULT_DATA_DIR));
        assert!(!c.debug);
        assert_eq!(c.data_dir_layer(None), Layer::Default);
    }

    #[test]
    fn cli_overrides_everything() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var("INIT_PRO_DATA_DIR", "/from/env");
        let c = Config::resolve(Some(Path::new("/from/cli")), Some(true));
        assert_eq!(c.data_dir, PathBuf::from("/from/cli"));
        assert!(c.debug);
        assert_eq!(c.data_dir_layer(Some(Path::new("/from/cli"))), Layer::Cli);
        std::env::remove_var("INIT_PRO_DATA_DIR");
    }

    #[test]
    fn env_overrides_default_but_not_cli() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var("INIT_PRO_DATA_DIR", "/from/env");
        let c = Config::resolve(None, None);
        assert_eq!(c.data_dir, PathBuf::from("/from/env"));
        assert_eq!(c.data_dir_layer(None), Layer::Env);

        let c2 = Config::resolve(Some(Path::new("/from/cli")), None);
        assert_eq!(c2.data_dir, PathBuf::from("/from/cli"));
        std::env::remove_var("INIT_PRO_DATA_DIR");
    }

    #[test]
    fn debug_flag_resolution() {
        let _g = ENV_LOCK.lock().unwrap();
        for v in ["1", "true", "TRUE", "yes", "on"] {
            std::env::set_var("INIT_PRO_DEBUG", v);
            assert!(
                Config::resolve(None, None).debug,
                "debug should be true for {v}"
            );
        }
        for v in ["0", "false", "no", ""] {
            std::env::set_var("INIT_PRO_DEBUG", v);
            assert!(
                !Config::resolve(None, None).debug,
                "debug should be false for {v:?}"
            );
        }
        std::env::remove_var("INIT_PRO_DEBUG");

        // Explicit CLI false beats env true.
        std::env::set_var("INIT_PRO_DEBUG", "true");
        assert!(!Config::resolve(None, Some(false)).debug);
        std::env::remove_var("INIT_PRO_DEBUG");
    }

    #[test]
    fn bin_dir_is_data_dir_bin() {
        let c = Config::resolve(Some(Path::new("/x")), None);
        assert_eq!(c.bin_dir(), PathBuf::from("/x/bin"));
    }
}
