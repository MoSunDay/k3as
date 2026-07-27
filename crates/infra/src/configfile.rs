//! Config-file pre-scan layer (Q8, TODO **T0.4**).
//!
//! Ports k3s's `pkg/configfilearg` semantics to a **pre-clap** concern:
//! locate a config file, read its `key=value` lines, and surface the file
//! layer to [`crate::Config`]. Resolution order mirrors k3s:
//!
//! `INIT_PRO_CONFIG_FILE` (env) → `--config`/`-c` (CLI) →
//! `<data-dir>/config.yaml` (default).
//!
//! v1 scope (per Q8 decision B): file + env + `--config`/`-c` + `key+`
//! append semantics. `.d/` dropins and http-config sources are explicitly
//! deferred.

use std::path::{Path, PathBuf};

/// One parsed `key value` entry from a config file.
///
/// `append == true` means the key was written with a trailing `+`
/// (`key+ value` / `key+=value` / `key+: value`) and the value must be
/// appended to the slice for that key rather than replacing it (k3s
/// `parser.go:279-294`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigEntry {
    pub key: String,
    pub value: String,
    pub append: bool,
}

/// Resolve which config file to read, honoring k3s precedence.
///
/// `locator_data_dir` is the data-dir determined *without* the file layer
/// (CLI > env > default); the default config path lives under it. This is
/// the first pass of the two-pass resolution that breaks the
/// data-dir↔config-path circular dependency (R3): the file may set a new
/// data-dir, but that value is never used to re-locate the config file.
///
/// Returns `None` when the resolved path does not exist.
pub fn resolve_path(cli_config: Option<&Path>, locator_data_dir: &Path) -> Option<PathBuf> {
    let resolved = if let Some(v) = std::env::var_os("INIT_PRO_CONFIG_FILE") {
        if v.is_empty() {
            None
        } else {
            Some(PathBuf::from(v))
        }
    } else if let Some(p) = cli_config {
        Some(p.to_path_buf())
    } else {
        Some(locator_data_dir.join("config.yaml"))
    };

    resolved.filter(|p| p.is_file())
}

/// Read and parse a config file. Returns `None` if it cannot be read.
pub fn load(path: &Path) -> Option<Vec<ConfigEntry>> {
    let content = std::fs::read_to_string(path).ok()?;
    Some(parse(&content))
}

/// Parse config-file text into entries.
///
/// Grammar (one directive per line): an optional leading comment (`#`),
/// then a key, optionally suffixed with `+` (append), then a separator
/// (space, `=`, or `:`) and a value. A bare key with no value yields an
/// empty value (bool flag parity). Surrounding quotes on the value are
/// stripped.
pub fn parse(content: &str) -> Vec<ConfigEntry> {
    let mut out = Vec::new();
    for raw in content.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // Find the first separator: space, '=' or ':'.
        let sep = line.find([' ', '=', ':']).unwrap_or(line.len());
        let mut key = line[..sep].trim().to_string();
        let rest = line[sep..].trim_start_matches([' ', '=', ':']).trim();
        if key.is_empty() {
            continue;
        }
        let append = key.ends_with('+');
        if append {
            key.pop();
        }
        out.push(ConfigEntry {
            key,
            value: strip_quotes(rest),
            append,
        });
    }
    out
}

/// Strip a single layer of matching surrounding quotes.
fn strip_quotes(v: &str) -> String {
    let b = v.as_bytes();
    if b.len() >= 2 && ((b[0] == b'"') || (b[0] == b'\'')) && b[0] == b[b.len() - 1] {
        v[1..b.len() - 1].to_string()
    } else {
        v.to_string()
    }
}

/// Last-wins scalar value for `key` (ignoring `key+` append entries).
///
/// Use for single-valued settings like `data-dir` / `debug`.
pub fn scalar<'a>(entries: &'a [ConfigEntry], key: &str) -> Option<&'a str> {
    entries
        .iter()
        .rev()
        .find(|e| !e.append && e.key == key)
        .map(|e| e.value.as_str())
}

/// Append-aware slice value for `key`.
///
/// A non-append entry resets the accumulated values; subsequent `key+`
/// entries append to it. Returns the values in order (k3s `key+` parity).
pub fn slice(entries: &[ConfigEntry], key: &str) -> Vec<String> {
    let mut acc: Vec<String> = Vec::new();
    for e in entries.iter().filter(|e| e.key == key) {
        if e.append {
            acc.push(e.value.clone());
        } else {
            acc.clear();
            if !e.value.is_empty() {
                acc.push(e.value.clone());
            }
        }
    }
    acc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_key_value_space_eq_colon() {
        let e = parse("data-dir /a\ndebug=true\nx: y\n");
        assert_eq!(e.len(), 3);
        assert_eq!(e[0].key, "data-dir");
        assert_eq!(e[0].value, "/a");
        assert_eq!(e[1].key, "debug");
        assert_eq!(e[1].value, "true");
        assert_eq!(e[2].key, "x");
        assert_eq!(e[2].value, "y");
    }

    #[test]
    fn parse_ignores_comments_and_blanks() {
        let e = parse("# comment\n\n   \n  # indented\nk v\n");
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].key, "k");
    }

    #[test]
    fn parse_bare_bool_key() {
        let e = parse("debug\n");
        assert_eq!(e.len(), 1);
        assert!(!e[0].append);
        assert_eq!(e[0].value, "");
    }

    #[test]
    fn parse_keyplus_append() {
        let e = parse("disable+ coredns\ndisable+ servicelb\n");
        assert_eq!(e.len(), 2);
        assert!(e[0].append && e[1].append);
        assert_eq!(e[0].key, "disable");
    }

    #[test]
    fn parse_strips_quotes() {
        let e = parse("data-dir \"/a b\"\ndebug='true'\n");
        assert_eq!(e[0].value, "/a b");
        assert_eq!(e[1].value, "true");
    }

    #[test]
    fn scalar_last_wins() {
        let e = parse("data-dir /a\ndata-dir /b\n");
        assert_eq!(scalar(&e, "data-dir"), Some("/b"));
        assert_eq!(scalar(&e, "missing"), None);
    }

    #[test]
    fn slice_reset_then_append() {
        let e = parse("disable a\ndisable+ b\ndisable c\ndisable+ d\n");
        assert_eq!(slice(&e, "disable"), vec!["c".to_string(), "d".to_string()]);
    }

    #[test]
    fn slice_append_only() {
        let e = parse("disable+ a\ndisable+ b\n");
        assert_eq!(slice(&e, "disable"), vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn scalar_ignores_append_entries() {
        let e = parse("disable+ a\n");
        assert_eq!(scalar(&e, "disable"), None);
    }
}
