//! Pre-clap no-op flag strip filter + one-time WARN deduper (Q9, A3).
//!
//! accept-no-op-warn flags (Table C) are identified and removed from argv
//! *before* clap parses, so the clap surface stays at the wired flags (R1)
//! and operators' k3s scripts keep working. Each distinct no-op flag seen is
//! logged exactly once at WARN. Stripping is subcommand-aware: since T4.2,
//! `--node-name` is wired for `agent` (kept for clap) while staying a no-op
//! for `server`/`stage`.

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

/// Agent-wired noop-table flags: kept for clap when the subcommand is
/// `agent`, still stripped (and warned) everywhere else. `node-name` since
/// T4.2; `node-ip` since Scope C (Q31); the flannel-* trio since T4.3
/// Scope A (Q34).
fn wired_for_agent(long: &str) -> bool {
    matches!(
        long,
        "node-name" | "node-ip" | "flannel-backend" | "flannel-iface" | "flannel-conf"
    )
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
    let sub = detect_subcommand(&tokens);
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
                // T4.2: `--node-name`, and since Scope C `--node-ip`, are
                // wired for `agent` — keep them so clap sees them; both stay
                // accept-no-op-warn for `server`/`stage`.
                if !(sub == Some("agent") && wired_for_agent(f.long)) {
                    record_seen(&mut seen, f.long);
                    if f.takes_value && !tok.contains('=') {
                        // Consume the separate value token (if present).
                        i += 2;
                    } else {
                        i += 1;
                    }
                    continue;
                }
            }
            out.push(tokens[i].clone());
            i += 1;
        } else if tok.len() > 1 && tok.starts_with('-') {
            let c = tok.chars().nth(1).unwrap();
            if let Some(f) = find_short(c) {
                // Same agent wiring for the short form: k3s spells
                // `--node-ip` as `-i` (only node-ip has an agent-wired short).
                if !(sub == Some("agent") && wired_for_agent(f.long)) {
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

/// Top-level subcommands (the multicall dispatcher keys off the same set).
const SUBCOMMANDS: &[&str] = &["server", "agent", "stage"];

/// First known subcommand in the token stream, skipping flags and their
/// values (globals may precede the subcommand, e.g. `--data-dir /x agent`).
/// Needed because stripping is subcommand-aware since T4.2: `--node-name`
/// is wired for `agent` but stays accept-no-op-warn elsewhere.
fn detect_subcommand(tokens: &[&String]) -> Option<&'static str> {
    let mut i = 0;
    while i < tokens.len() {
        let tok = tokens[i].as_str();
        if tok == "--" {
            return None;
        }
        if let Some(rest) = tok.strip_prefix("--") {
            if !rest.contains('=') {
                // `--flag VALUE`: skip the separate value token.
                let takes_value = rest == "data-dir"
                    || rest == "cluster-cidr"
                    || find_long(rest).is_some_and(|f| f.takes_value);
                if takes_value {
                    i += 1;
                }
            }
        } else if tok.len() > 1 && tok.starts_with('-') {
            let c = tok.chars().nth(1).unwrap();
            let attached = tok.len() > 2; // -dVAL / -d=VAL carry the value.
            let takes_value = c == 'd' || find_short(c).is_some_and(|f| f.takes_value);
            if takes_value && !attached {
                i += 1;
            }
        } else {
            for s in SUBCOMMANDS {
                if tok == *s {
                    return Some(*s);
                }
            }
            return None; // unknown positional: not a subcommand we route.
        }
        i += 1;
    }
    None
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
        let r = strip_noop(&argv(&[
            "init-pro",
            "server",
            "--service-cidr",
            "10.0.0.0/16",
        ]));
        assert_eq!(r.argv, argv(&["init-pro", "server"]));
        assert_eq!(r.seen, vec!["service-cidr".to_string()]);
    }

    #[test]
    fn equals_form_keeps_no_value_token() {
        let r = strip_noop(&argv(&["init-pro", "server", "--service-cidr=10.0.0.0/16"]));
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
            "init-pro",
            "server",
            "--data-dir",
            "/x",
            "--debug",
            "--cluster-init",
        ]));
        assert_eq!(
            r.argv,
            argv(&[
                "init-pro",
                "server",
                "--data-dir",
                "/x",
                "--debug",
                "--cluster-init"
            ])
        );
        assert!(r.seen.is_empty());
    }

    #[test]
    fn mixed_noop_and_wired() {
        // T4.2: `--node-name` is wired for `agent` — it must reach clap.
        let r = strip_noop(&argv(&[
            "init-pro",
            "agent",
            "--token",
            "secret",
            "--node-name",
            "n1",
            "--rootless",
            "--debug",
        ]));
        assert_eq!(
            r.argv,
            argv(&[
                "init-pro",
                "agent",
                "--token",
                "secret",
                "--node-name",
                "n1",
                "--debug"
            ])
        );
        assert_eq!(r.seen, vec!["rootless".to_string()]);
    }

    #[test]
    fn node_name_stays_noop_outside_agent() {
        // `server`/`stage` keep the accept-no-op-warn behavior (Table C).
        for sub in ["server", "stage"] {
            let r = strip_noop(&argv(&["init-pro", sub, "--node-name", "n1"]));
            assert_eq!(r.argv, argv(&["init-pro", sub]));
            assert_eq!(r.seen, vec!["node-name".to_string()]);
        }
    }

    #[test]
    fn node_name_wired_with_globals_before_subcommand() {
        // Subcommand detection must skip the `--data-dir` value token.
        let r = strip_noop(&argv(&[
            "init-pro",
            "--data-dir",
            "/dd",
            "agent",
            "--node-name",
            "n1",
        ]));
        assert_eq!(
            r.argv,
            argv(&[
                "init-pro",
                "--data-dir",
                "/dd",
                "agent",
                "--node-name",
                "n1"
            ])
        );
        assert!(r.seen.is_empty());
    }

    #[test]
    fn node_ip_wired_for_agent_stripped_elsewhere() {
        // T4.2 Scope C (Q31): `--node-ip` reaches clap on `agent` (both
        // separate-value and inline `=` forms), stays no-op on server/stage,
        // and its k3s short form `-i` is kept for agent too.
        let r = strip_noop(&argv(&["init-pro", "agent", "--node-ip", "10.0.0.7"]));
        assert_eq!(
            r.argv,
            argv(&["init-pro", "agent", "--node-ip", "10.0.0.7"])
        );
        assert!(r.seen.is_empty());

        let r = strip_noop(&argv(&["init-pro", "agent", "--node-ip=10.0.0.8"]));
        assert_eq!(r.argv, argv(&["init-pro", "agent", "--node-ip=10.0.0.8"]));
        assert!(r.seen.is_empty());

        let r = strip_noop(&argv(&["init-pro", "agent", "-i", "10.0.0.9"]));
        assert_eq!(r.argv, argv(&["init-pro", "agent", "-i", "10.0.0.9"]));
        assert!(r.seen.is_empty());

        for sub in ["server", "stage"] {
            let r = strip_noop(&argv(&["init-pro", sub, "--node-ip", "10.0.0.7"]));
            assert_eq!(r.argv, argv(&["init-pro", sub]));
            assert_eq!(r.seen, vec!["node-ip".to_string()]);
        }
    }

    #[test]
    fn flannel_flags_wired_for_agent_stripped_elsewhere() {
        // T4.3 Scope A (Q34): the flannel-* trio reaches clap on `agent`
        // (drives net-conf.json + flanneld Options), stays accept-no-op-warn
        // on server/stage.
        let r = strip_noop(&argv(&[
            "init-pro",
            "agent",
            "--flannel-backend",
            "host-gw",
            "--flannel-iface=eth1",
            "--flannel-conf",
            "/etc/flannel/net-conf.json",
        ]));
        assert_eq!(
            r.argv,
            argv(&[
                "init-pro",
                "agent",
                "--flannel-backend",
                "host-gw",
                "--flannel-iface=eth1",
                "--flannel-conf",
                "/etc/flannel/net-conf.json"
            ])
        );
        assert!(r.seen.is_empty());

        let r = strip_noop(&argv(&["init-pro", "server", "--flannel-backend", "vxlan"]));
        assert_eq!(r.argv, argv(&["init-pro", "server"]));
        assert_eq!(r.seen, vec!["flannel-backend".to_string()]);
    }

    #[test]
    fn cluster_cidr_never_stripped_and_value_not_mistaken_for_subcommand() {
        // T4.3 Scope A: `--cluster-cidr` is wired (Table A) on both
        // subcommands — clap sees it untouched, and its VALUE token must
        // not blind detect_subcommand (a wired flag before the subcommand).
        // `--flannel-backend` after the subcommand follows the usual
        // subcommand-aware rule: kept for `agent` (wired there), nooped for
        // `server` (the embedded flanneld runs agent-side, Q34).
        for (sub, kept) in [("server", false), ("agent", true)] {
            let r = strip_noop(&argv(&[
                "init-pro",
                "--cluster-cidr",
                "10.42.0.0/16",
                sub,
                "--flannel-backend",
                "vxlan",
            ]));
            let mut expected = vec![
                "init-pro".to_string(),
                "--cluster-cidr".to_string(),
                "10.42.0.0/16".to_string(),
                sub.to_string(),
            ];
            if kept {
                expected.push("--flannel-backend".to_string());
                expected.push("vxlan".to_string());
                assert!(r.seen.is_empty());
            } else {
                assert_eq!(r.seen, vec!["flannel-backend".to_string()]);
            }
            assert_eq!(r.argv, expected);
        }
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
        assert_eq!(
            r.argv,
            argv(&["init-pro", "server", "--totally-unknown", "x"])
        );
        assert!(r.seen.is_empty());
    }

    #[test]
    fn bool_noop_at_value_position_not_swallowed() {
        // A bool no-op followed by a wired value flag: the wired flag survives.
        let r = strip_noop(&argv(&[
            "init-pro",
            "server",
            "--rootless",
            "--data-dir",
            "/x",
        ]));
        assert_eq!(r.argv, argv(&["init-pro", "server", "--data-dir", "/x"]));
    }
}
