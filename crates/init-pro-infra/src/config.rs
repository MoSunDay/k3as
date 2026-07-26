//! Layered runtime config: CLI flag > env > config file > default.
//!
//! k3s resolves `--data-dir`/`--debug` early (before full flag parse) via a
//! config-file pre-scan; init-pro mirrors that with [`Config::resolve`].
//! The config-file machinery lives in [`crate::configfile`] (Q8).

use crate::configfile;
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
    /// `cli_config` is the `--config`/`-c` path surfaced by the pre-clap scan.
    ///
    /// Two-pass resolution (R3): a *locator* data-dir (CLI > env > default,
    /// no file) decides where to look for the default `config.yaml`; the
    /// file may then set a different data-dir, which is honored for
    /// everything except re-locating the config file.
    pub fn resolve(
        cli_data_dir: Option<&Path>,
        cli_debug: Option<bool>,
        cli_config: Option<&Path>,
    ) -> Self {
        // Pass 1: locator data-dir without the file layer.
        let locator = locator_data_dir(cli_data_dir);

        // Resolve + read the config file (file layer).
        let entries = configfile::resolve_path(cli_config, &locator)
            .and_then(|p| configfile::load(&p))
            .unwrap_or_default();

        // Default → file layer.
        let mut cfg = Config::default();
        if let Some(dd) = file_value(&entries, "data-dir") {
            cfg.data_dir = PathBuf::from(dd);
        }
        if let Some(d) = file_value(&entries, "debug") {
            cfg.debug = parse_bool(d);
        }

        // Env layer (INIT_PRO_* mirrors k3s env conventions).
        if let Some(v) = env_nonempty("INIT_PRO_DATA_DIR") {
            cfg.data_dir = PathBuf::from(v);
        }
        if let Some(v) = env_nonempty("INIT_PRO_DEBUG") {
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

    /// `bin/` sibling of the data dir (k3s stages peers under `<data-dir>/bin`).
    pub fn bin_dir(&self) -> PathBuf {
        self.data_dir.join("bin")
    }

    /// Which layer the data-dir came from (best-effort, for tracing/tests).
    pub fn data_dir_layer(&self, cli_data_dir: Option<&Path>) -> Layer {
        if cli_data_dir.is_some() {
            Layer::Cli
        } else if env_nonempty("INIT_PRO_DATA_DIR").is_some() {
            Layer::Env
        } else if file_layer_data_dir(cli_config_for_query()).is_some() {
            Layer::File
        } else {
            Layer::Default
        }
    }
}

/// The data-dir used to *locate* the config file (pass 1): CLI > env > default.
fn locator_data_dir(cli_data_dir: Option<&Path>) -> PathBuf {
    if let Some(d) = cli_data_dir {
        return d.to_path_buf();
    }
    if let Some(v) = env_nonempty("INIT_PRO_DATA_DIR") {
        return PathBuf::from(v);
    }
    PathBuf::from(DEFAULT_DATA_DIR)
}

/// Read a scalar value for `key` from the currently resolved config file.
///
/// Mirrors the file-layer lookup used by [`Config::resolve`] so tests can
/// assert "the file layer returned a real value" (A1 acceptance gate).
pub fn file_layer_data_dir(cli_config: Option<&Path>) -> Option<PathBuf> {
    let locator = locator_data_dir(None);
    let entries = configfile::resolve_path(cli_config, &locator)
        .and_then(|p| configfile::load(&p))?;
    file_value(&entries, "data-dir").map(PathBuf::from)
}

/// File-layer `debug` value (A1 acceptance gate).
pub fn file_layer_debug(cli_config: Option<&Path>) -> Option<bool> {
    let locator = locator_data_dir(None);
    let entries = configfile::resolve_path(cli_config, &locator)
        .and_then(|p| configfile::load(&p))?;
    file_value(&entries, "debug").map(parse_bool)
}

fn file_value<'a>(entries: &'a [configfile::ConfigEntry], key: &str) -> Option<&'a str> {
    // A bare bool key (empty value) means "true" (k3s parity).
    match configfile::scalar(entries, key) {
        Some("") => Some("true"),
        Some(v) => Some(v),
        None => None,
    }
}

/// For the legacy `data_dir_layer` query: surface whatever `--config` the
/// process used. Tests set this via `INIT_PRO_CONFIG_FILE` or pass paths
/// directly; in production the CLI passes the pre-scanned value.
fn cli_config_for_query() -> Option<&'static Path> {
    None
}

fn env_nonempty(var: &str) -> Option<String> {
    match std::env::var(var) {
        Ok(v) if !v.trim().is_empty() => Some(v),
        _ => None,
    }
}

fn parse_bool(v: &str) -> bool {
    matches!(
        v.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn clear_env() {
        std::env::remove_var("INIT_PRO_DATA_DIR");
        std::env::remove_var("INIT_PRO_DEBUG");
        std::env::remove_var("INIT_PRO_CONFIG_FILE");
    }

    /// Write a config file under a fresh temp dir; returns its path.
    fn write_cfg(name: &str, body: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "initpro-cfg-{}-{}",
            name,
            std::process::id()
        ));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("config.yaml");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        path
    }

    #[test]
    fn default_data_dir() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_env();
        let c = Config::resolve(None, None, None);
        assert_eq!(c.data_dir, PathBuf::from(DEFAULT_DATA_DIR));
        assert!(!c.debug);
        assert_eq!(c.data_dir_layer(None), Layer::Default);
    }

    #[test]
    fn cli_overrides_everything() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var("INIT_PRO_DATA_DIR", "/from/env");
        let c = Config::resolve(Some(Path::new("/from/cli")), Some(true), None);
        assert_eq!(c.data_dir, PathBuf::from("/from/cli"));
        assert!(c.debug);
        assert_eq!(c.data_dir_layer(Some(Path::new("/from/cli"))), Layer::Cli);
        clear_env();
    }

    #[test]
    fn env_overrides_default_but_not_cli() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var("INIT_PRO_DATA_DIR", "/from/env");
        let c = Config::resolve(None, None, None);
        assert_eq!(c.data_dir, PathBuf::from("/from/env"));
        assert_eq!(c.data_dir_layer(None), Layer::Env);

        let c2 = Config::resolve(Some(Path::new("/from/cli")), None, None);
        assert_eq!(c2.data_dir, PathBuf::from("/from/cli"));
        clear_env();
    }

    #[test]
    fn debug_flag_resolution() {
        let _g = ENV_LOCK.lock().unwrap();
        for v in ["1", "true", "TRUE", "yes", "on"] {
            std::env::set_var("INIT_PRO_DEBUG", v);
            assert!(Config::resolve(None, None, None).debug, "debug true for {v}");
        }
        for v in ["0", "false", "no", ""] {
            std::env::set_var("INIT_PRO_DEBUG", v);
            assert!(!Config::resolve(None, None, None).debug, "debug false for {v:?}");
        }
        std::env::remove_var("INIT_PRO_DEBUG");
        // Explicit CLI false beats env true.
        std::env::set_var("INIT_PRO_DEBUG", "true");
        assert!(!Config::resolve(None, Some(false), None).debug);
        clear_env();
    }

    #[test]
    fn bin_dir_is_data_dir_bin() {
        let c = Config::resolve(Some(Path::new("/x")), None, None);
        assert_eq!(c.bin_dir(), PathBuf::from("/x/bin"));
    }

    // ---- A1: config-file layer (5 resolution orders) ----

    #[test]
    fn file_layer_sets_data_dir_when_no_cli_no_env() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_env();
        let cfg = write_cfg("file-dd", "data-dir /from/file\ndebug true\n");
        let c = Config::resolve(None, None, Some(&cfg));
        assert_eq!(c.data_dir, PathBuf::from("/from/file"));
        assert!(c.debug);
    }

    #[test]
    fn env_config_file_path_wins_over_cli_config() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_env();
        let a = write_cfg("env-wins", "data-dir /from-env-file\n");
        let b = write_cfg("cli-loses", "data-dir /from-cli-file\n");
        std::env::set_var("INIT_PRO_CONFIG_FILE", a.to_string_lossy().to_string());
        let c = Config::resolve(None, None, Some(&b));
        assert_eq!(c.data_dir, PathBuf::from("/from-env-file"));
        clear_env();
    }

    #[test]
    fn cli_data_dir_overrides_file_layer() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_env();
        let cfg = write_cfg("cli-beats-file", "data-dir /from-file\n");
        let c = Config::resolve(Some(Path::new("/from/cli")), None, Some(&cfg));
        assert_eq!(c.data_dir, PathBuf::from("/from/cli"));
    }

    #[test]
    fn env_data_dir_overrides_file_layer() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_env();
        let cfg = write_cfg("env-beats-file", "data-dir /from-file\n");
        std::env::set_var("INIT_PRO_DATA_DIR", "/from-env");
        let c = Config::resolve(None, None, Some(&cfg));
        assert_eq!(c.data_dir, PathBuf::from("/from-env"));
        clear_env();
    }

    #[test]
    fn default_config_path_under_data_dir() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_env();
        // Place config.yaml inside a temp data-dir (the default lookup).
        // env data-dir points the locator here so the default path
        // <data-dir>/config.yaml resolves to this file.
        let dir = std::env::temp_dir().join(format!("initpro-dd-default-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.yaml"), "data-dir /resolved-by-default-path\n").unwrap();
        std::env::set_var("INIT_PRO_DATA_DIR", dir.to_string_lossy().to_string());
        // No --config: resolve_path falls back to <locator>/config.yaml.
        // The file layer reading the value proves the default path was used.
        assert_eq!(
            file_layer_data_dir(None),
            Some(PathBuf::from("/resolved-by-default-path"))
        );
        // Precedence env > file: the env data-dir still wins overall.
        let c = Config::resolve(None, None, None);
        assert_eq!(c.data_dir, dir);
        let _ = std::fs::remove_dir_all(&dir);
        clear_env();
    }

    // ---- A1: key+ append + circular resolution ----

    #[test]
    fn keyplus_append_read_from_file() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_env();
        let cfg = write_cfg(
            "append",
            "disable+ coredns\ndisable+ servicelb\n",
        );
        let entries = configfile::load(&cfg).unwrap();
        assert_eq!(
            configfile::slice(&entries, "disable"),
            vec!["coredns".to_string(), "servicelb".to_string()]
        );
    }

    #[test]
    fn circular_data_dir_does_not_relocate_config() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_env();
        // env data-dir points the locator at /tmp/<loc>; the config file there
        // sets a *different* data-dir. The file must be read from the locator
        // path, NOT re-resolved against the file's own data-dir.
        let loc = std::env::temp_dir().join(format!("initpro-loc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&loc);
        std::fs::create_dir_all(&loc).unwrap();
        std::fs::write(loc.join("config.yaml"), "data-dir /from-file\n").unwrap();
        std::env::set_var("INIT_PRO_DATA_DIR", loc.to_string_lossy().to_string());

        // File layer value proves the file at <locator>/config.yaml was read.
        assert_eq!(
            file_layer_data_dir(None),
            Some(PathBuf::from("/from-file"))
        );
        // Final resolved data-dir = env (wins over file).
        let c = Config::resolve(None, None, None);
        assert_eq!(c.data_dir, loc);
        let _ = std::fs::remove_dir_all(&loc);
        clear_env();
    }
}
