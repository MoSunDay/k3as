//! Pre-clap no-op flag strip filter + one-time WARN deduper (Q9, A3).
//!
//! accept-no-op-warn flags (Table C) are identified and removed from argv
//! *before* clap parses, so the clap surface stays at the 17 wired flags (R1)
//! and operators' k3s scripts keep working. Each distinct no-op flag seen is
//! logged exactly once at WARN.

pub mod conflicts;
pub mod noop;

pub use noop::{find_long, find_short};

/// Result of stripping no-op flags from argv.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct StripResult {
    /// argv with no-op flags (and their values) removed — safe to feed clap.
    pub argv: Vec<String>,
    /// Distinct no-op flag long names seen, in first-seen order.
    pub seen: Vec<String>,
}

/// Strip accept-no-op-warn flags from `argv` (argv[0] is preserved as-is).
///
/// Stops stripping at `--` (end of options); everything after is passed
/// through verbatim. Recognizes `--long`, `--long=val`, `-x`, `-x=val`, and
/// `-xval` forms for value-taking flags.
pub fn strip_noop(argv: &[String]) -> StripResult {
    let mut out = Vec::with_capacity(argv.len());
    let mut seen: Vec<String> = Vec::new();
    // Always keep argv[0] (program name).
    let mut iter = argv.iter();
    if let Some(prog) = iter.next() {
        out.push(prog.clone());
    }

    let tokens: Vec<&String> = iter.collect();
    let mut i = 0;
    while i < tokens.len() {
        let tok = tokens[i].as_str();
        if tok == "--" {
            // End of options: pass the separator and all remaining tokens.
            out.push(tokens[i].clone());
            i += 1;
            while i < tokens.len() {
                out.push(tokens[i].clone());
                i += 1;
            }
            break;
        }
        if let Some(rest) = tok.strip_prefix("--") {
            let (name, _has_eq_val) = match rest.split_once('=') {
                Some((n, _v)) => (n, true),
                None => (rest, false),
            };
            if let Some(f) = find_long(name) {
                record_seen(&mut seen, f.long);
                if f.takes_value && !tok.contains('=') {
                    // Consume the separate value token (if present).
                    i += 2;
                } else {
                    i += 1;
                }
                continue;
            }
            out.push(tokens[i].clone());
            i += 1;
        } else if tok.len() > 1 && tok.starts_with('-') {
            let c = tok.chars().nth(1).unwrap();
            if let Some(f) = find_short(c) {
                record_seen(&mut seen, f.long);
                let after = &tok[2..];
                if f.takes_value {
                    // -x=val / -xval: value is attached → skip one token.
                    // -x with nothing after: value is the next token.
                    if after.is_empty() || after == "=" {
                        i += 2;
                    } else {
                        i += 1;
                    }
                } else {
                    i += 1;
                }
                continue;
            }
            out.push(tokens[i].clone());
            i += 1;
        } else {
            out.push(tokens[i].clone());
            i += 1;
        }
    }

    StripResult { argv: out, seen }
}

/// Log each seen no-op flag once at WARN (deduped by construction).
pub fn warn_noops(seen: &[String]) {
    for flag in seen {
        tracing::warn!(
            target: "init-pro",
            "flag `{flag}` accepted but not yet implemented; no-op"
        );
    }
}

fn record_seen(seen: &mut Vec<String>, long: &str) {
    if !seen.iter().any(|s| s == long) {
        seen.push(long.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn bool_noop_flag_stripped_no_value() {
        let r = strip_noop(&argv(&["init-pro", "server", "--rootless"]));
        assert_eq!(r.argv, argv(&["init-pro", "server"]));
        assert_eq!(r.seen, vec!["rootless".to_string()]);
    }

    #[test]
    fn value_noop_flag_strips_value() {
        let r = strip_noop(&argv(&["init-pro", "server", "--cluster-cidr", "10.0.0.0/16"]));
        assert_eq!(r.argv, argv(&["init-pro", "server"]));
        assert_eq!(r.seen, vec!["cluster-cidr".to_string()]);
    }

    #[test]
    fn equals_form_keeps_no_value_token() {
        let r = strip_noop(&argv(&["init-pro", "server", "--cluster-cidr=10.0.0.0/16"]));
        assert_eq!(r.argv, argv(&["init-pro", "server"]));
    }

    #[test]
    fn short_value_flag_consumes_next() {
        let r = strip_noop(&argv(&["init-pro", "server", "-l", "/var/log/x"]));
        assert_eq!(r.argv, argv(&["init-pro", "server"]));
        assert_eq!(r.seen, vec!["log".to_string()]);
    }

    #[test]
    fn short_attached_value() {
        let r = strip_noop(&argv(&["init-pro", "server", "-l/var/log/x"]));
        assert_eq!(r.argv, argv(&["init-pro", "server"]));
    }

    #[test]
    fn wired_flags_pass_through() {
        let r = strip_noop(&argv(&[
            "init-pro", "server", "--data-dir", "/x", "--debug", "--cluster-init",
        ]));
        assert_eq!(r.argv, argv(&["init-pro", "server", "--data-dir", "/x", "--debug", "--cluster-init"]));
        assert!(r.seen.is_empty());
    }

    #[test]
    fn mixed_noop_and_wired() {
        let r = strip_noop(&argv(&[
            "init-pro", "agent", "--token", "secret", "--node-name", "n1", "--debug",
        ]));
        assert_eq!(
            r.argv,
            argv(&["init-pro", "agent", "--token", "secret", "--debug"])
        );
        assert_eq!(r.seen, vec!["node-name".to_string()]);
    }

    #[test]
    fn repeated_same_flag_deduped_once() {
        let r = strip_noop(&argv(&["init-pro", "server", "--v", "1", "--v", "2"]));
        assert_eq!(r.seen, vec!["v".to_string()]);
    }

    #[test]
    fn double_dash_stops_stripping() {
        let r = strip_noop(&argv(&["init-pro", "server", "--", "--rootless"]));
        assert_eq!(r.argv, argv(&["init-pro", "server", "--", "--rootless"]));
        assert!(r.seen.is_empty());
    }

    #[test]
    fn unknown_flag_passes_to_clap() {
        let r = strip_noop(&argv(&["init-pro", "server", "--totally-unknown", "x"]));
        assert_eq!(r.argv, argv(&["init-pro", "server", "--totally-unknown", "x"]));
        assert!(r.seen.is_empty());
    }

    #[test]
    fn bool_noop_at_value_position_not_swallowed() {
        // A bool no-op followed by a wired value flag: the wired flag survives.
        let r = strip_noop(&argv(&["init-pro", "server", "--rootless", "--data-dir", "/x"]));
        assert_eq!(r.argv, argv(&["init-pro", "server", "--data-dir", "/x"]));
    }
}
