//! Pre-clap `--config`/`-c` scanner (Q8).
//!
//! k3s pre-scans argv for the config-file path *before* the flag parser
//! runs (`pkg/configfilearg`). init-pro mirrors this so the file layer can
//! be resolved without depending on clap. Only the config path is surfaced
//! here; `--data-dir`/`--debug` still come from clap (they drive the
//! locator + logging after parse).
//!
//! Short-circuits to `None` on `--help`/`-h`/`--version`/`-v` (k3s
//! `MustFindString` parity) — those never reach config resolution.

use std::path::PathBuf;

/// Tokens that short-circuit the scan (k3s parity).
const SHORT_CIRCUIT: &[&str] = &["--help", "-h", "--version", "-v"];

/// Scan argv (excluding `argv[0]`) for a `--config`/`-c` value.
///
/// Recognizes `--config X`, `--config=X`, `-c X`, and `-cX`. Stops at `--`
/// (end of options). Returns the first match, or `None` if a short-circuit
/// token appears first.
pub fn pre_scan_config(argv: &[String]) -> Option<PathBuf> {
    // Pass 1: if a short-circuit token appears anywhere before `--`, k3s
    // bails out of config resolution entirely (the user wants help/version).
    if has_short_circuit(argv) {
        return None;
    }
    // Pass 2: find the first --config/-c value.
    let mut i = 1; // skip argv[0]
    while i < argv.len() {
        let tok = argv[i].as_str();
        if tok == "--" {
            return None;
        }
        if tok == "--config" || tok == "-c" {
            if i + 1 < argv.len() {
                return Some(PathBuf::from(&argv[i + 1]));
            }
            return None;
        }
        if let Some(v) = tok.strip_prefix("--config=") {
            return Some(PathBuf::from(v));
        }
        if let Some(v) = tok.strip_prefix("-c") {
            if !v.is_empty() {
                return Some(PathBuf::from(v));
            }
        }
        i += 1;
    }
    None
}

/// True if a help/version token appears before the `--` separator.
fn has_short_circuit(argv: &[String]) -> bool {
    argv.iter()
        .skip(1)
        .take_while(|t| t.as_str() != "--")
        .any(|t| SHORT_CIRCUIT.contains(&t.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn argv(items: &[&str]) -> Vec<String> {
        std::iter::once("init-pro".to_string())
            .chain(items.iter().map(|s| s.to_string()))
            .collect()
    }

    #[test]
    fn long_form_with_space() {
        let a = argv(&["server", "--config", "/x/c.yaml"]);
        assert_eq!(pre_scan_config(&a), Some(PathBuf::from("/x/c.yaml")));
    }

    #[test]
    fn long_form_equals() {
        let a = argv(&["--config=/y/c.yaml"]);
        assert_eq!(pre_scan_config(&a), Some(PathBuf::from("/y/c.yaml")));
    }

    #[test]
    fn short_form_with_space() {
        let a = argv(&["server", "-c", "/z/c.yaml"]);
        assert_eq!(pre_scan_config(&a), Some(PathBuf::from("/z/c.yaml")));
    }

    #[test]
    fn short_form_attached() {
        let a = argv(&["server", "-cw/c.yaml"]);
        assert_eq!(pre_scan_config(&a), Some(PathBuf::from("w/c.yaml")));
    }

    #[test]
    fn none_when_absent() {
        let a = argv(&["server", "--debug"]);
        assert_eq!(pre_scan_config(&a), None);
    }

    #[test]
    fn stops_at_double_dash() {
        let a = argv(&["--", "--config", "/x/c.yaml"]);
        assert_eq!(pre_scan_config(&a), None);
    }

    #[test]
    fn short_circuits_on_help() {
        let a = argv(&["server", "--config", "/x/c.yaml", "--help"]);
        assert_eq!(pre_scan_config(&a), None);
        let b = argv(&["server", "-h", "--config", "/x/c.yaml"]);
        assert_eq!(pre_scan_config(&b), None);
    }

    #[test]
    fn short_circuits_on_version() {
        let a = argv(&["--version", "--config", "/x/c.yaml"]);
        assert_eq!(pre_scan_config(&a), None);
    }

    #[test]
    fn skips_argv0() {
        // argv[0] might literally be "--config" if invoked weirdly; we skip it.
        let a = vec!["--config".to_string(), "/should-not-match".to_string()];
        assert_eq!(pre_scan_config(&a), None);
    }

    #[test]
    fn pathbuf_matches() {
        let a = argv(&["--config", "/x/c.yaml"]);
        assert_eq!(pre_scan_config(&a).as_deref(), Some(Path::new("/x/c.yaml")));
    }
}
