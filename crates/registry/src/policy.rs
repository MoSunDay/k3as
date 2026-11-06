//! Tag-retention policy math (pure functions; TODO **T6.3**, Q40).
//!
//! The rule: per repository keep the newest `retain` tags by push time;
//! keep-forever patterns are EXCLUDED from the count and never pruned.
//! Patterns are `repo:tag` globs where `*` is a wildcard (e.g.
//! `myapp:stable*`, `myapp:1.0.*`, `myapp:*` = whole repo, `*` alone is
//! rejected as a footgun). Patterns come from two sources unioned on
//! every GC pass: the CLI flags (`--registry-keep-tag`) and the on-disk
//! `<data_dir>/registry/keep.json` array (hot-reloaded, no restart).

use std::path::{Path, PathBuf};

use crate::store::TagMeta;

/// Default newest-tags-to-keep per repository.
pub const DEFAULT_TAG_RETENTION: usize = 10;

/// `*`-glob match, anchored at both ends (a match must consume the
/// whole string). `*` is a wildcard spanning ANY run of chars -
/// including `/` and `:` - so it can cross the repo/tag separator.
/// Literals match greedily left-to-right, but the matcher backtracks
/// over the most recent `*` on mismatch, so a wildcard never causes a
/// false negative (`x*aa*a` matches `xaaa` - a pinned tag must not be
/// pruned just because a later literal also occurs inside the span).
pub fn glob_match(pattern: &str, s: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = s.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    // Anchor of the most recent `*`: its pattern index plus the text
    // offset it was matched at (the backtrack resume point).
    let mut star: Option<usize> = None;
    let mut mark = 0usize;
    while ti < t.len() {
        if pi < p.len() && p[pi] == t[ti] {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            pi += 1;
            mark = ti;
        } else if let Some(sp) = star {
            // Let the last `*` swallow one more char and retry.
            pi = sp + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    // Trailing `*`s match the empty suffix.
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// A keep-forever pattern applies to (repo, tag)? Accepts `repo:tag`
/// globs; a bare `repo` (no colon) keeps the whole repository.
pub fn is_forever(patterns: &[String], repo: &str, tag: &str) -> bool {
    patterns.iter().any(|p| {
        if let Some((pr, pt)) = p.split_once(':') {
            glob_match(pr, repo) && glob_match(pt, tag)
        } else {
            glob_match(p, repo)
        }
    })
}

/// `keep.json` path (live-edited; re-read on every GC pass).
pub fn keep_file(root: &Path) -> PathBuf {
    root.join("keep.json")
}

/// Union on-disk keep.json with CLI extras. Invalid entries are
/// skipped, never fatal (log-and-continue doctrine).
pub async fn load_keep(root: &Path, extra: &[String]) -> Vec<String> {
    let mut out: Vec<String> = extra.to_vec();
    if let Ok(raw) = tokio::fs::read(keep_file(root)).await {
        match serde_json::from_slice::<serde_json::Value>(&raw) {
            Ok(v) => {
                if let Some(arr) = v.as_array() {
                    for p in arr.iter().filter_map(|x| x.as_str()) {
                        if valid_pattern(p) {
                            out.push(p.to_string());
                        }
                    }
                }
            }
            Err(e) => {
                tracing::warn!(target: "init-pro", error = %e, "registry keep.json unparsable; ignored");
            }
        }
    }
    out.retain(|p| valid_pattern(p));
    out.dedup();
    out
}

/// `*` alone is rejected (it would disable retention entirely by
/// accident); anything else with at least one non-`*` char is fine.
fn valid_pattern(p: &str) -> bool {
    !p.trim_matches('*').is_empty()
}

/// Pure retention decision: which tags to drop so that exactly the
/// newest `retain` countable tags survive. Countable = not keep-forever.
/// `retain == 0` = unlimited. Ties on (pushed_at, seq) fall back to
/// reverse tag-name order so the plan is deterministic.
pub fn prune_plan(
    repo: &str,
    tags: &[(String, TagMeta)],
    patterns: &[String],
    retain: usize,
) -> Vec<String> {
    if retain == 0 {
        return Vec::new();
    }
    let mut countable: Vec<&(String, TagMeta)> = tags
        .iter()
        .filter(|(t, _)| !is_forever(patterns, repo, t))
        .collect();
    if countable.len() <= retain {
        return Vec::new();
    }
    countable.sort_by(|a, b| {
        let ka = (a.1.pushed_at, a.1.seq);
        let kb = (b.1.pushed_at, b.1.seq);
        kb.cmp(&ka).then_with(|| b.0.cmp(&a.0))
    });
    let mut drops: Vec<String> = countable[retain..].iter().map(|(t, _)| t.clone()).collect();
    drops.sort();
    drops
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(pushed_at: u64, seq: u64) -> TagMeta {
        TagMeta {
            digest: format!("sha256:{seq:064x}"),
            media_type: None,
            pushed_at,
            seq,
            size: 1,
        }
    }

    fn tags(n: u64) -> Vec<(String, TagMeta)> {
        (0..n).map(|i| (format!("v{i}"), meta(i, i))).collect()
    }

    #[test]
    fn glob_basics() {
        assert!(glob_match("stable*", "stable"));
        assert!(glob_match("stable*", "stable-1.2"));
        assert!(glob_match("1.0.*", "1.0.3"));
        assert!(!glob_match("1.0.*", "1.1.0"));
        assert!(glob_match("a*b*c", "aXXbYYc"));
        assert!(glob_match("exact", "exact"));
        assert!(!glob_match("exact", "exact2"));
        assert!(!glob_match("app:1*", "app:2.0"));
        // Backtracking: the greedy star must give chars back (B5).
        assert!(glob_match("x*aa*a", "xaaa"));
        assert!(!glob_match("x*aa*a", "xaa"));
        assert!(glob_match("app:release-*rc*1", "app:release-1.2rc1"));
        assert!(!glob_match("app:release-*rc*1", "app:release-1.2rc2"));
        assert!(glob_match("*", ""));
        // Cross-separator semantics unchanged: `*` spans `/` and `:`.
        assert!(glob_match("a*b", "a/b"));
        assert!(glob_match("*", "app:1.0.0"));
        // Anchored at both ends; no byte slicing, multibyte-safe.
        assert!(!glob_match("b*", "abc"));
        assert!(glob_match("café*", "café-1"));
        assert!(!glob_match("café*", "caf-1"));
    }

    #[test]
    fn forever_matching() {
        let pats = vec![
            "myapp:stable*".to_string(),
            "freeze:1.0.0".to_string(),
            "gold/*".to_string(),
        ];
        assert!(is_forever(&pats, "myapp", "stable-rc1"));
        assert!(!is_forever(&pats, "myapp", "latest"));
        assert!(is_forever(&pats, "freeze", "1.0.0"));
        assert!(is_forever(&pats, "gold/x", "anything"));
        assert!(!is_forever(&pats, "other", "1.0.0"));
    }

    #[test]
    fn keeps_newest_n() {
        let ts = tags(12);
        let drops = prune_plan("r", &ts, &[], 10);
        assert_eq!(drops, vec!["v0", "v1"]);
    }

    #[test]
    fn zero_retain_is_unlimited() {
        assert!(prune_plan("r", &tags(50), &[], 0).is_empty());
    }

    #[test]
    fn forever_excluded_from_count() {
        // 12 tags, but v0..v3 are keep-forever: only 8 count, nothing drops.
        let mut ts = tags(12);
        let pats = vec![
            "r:v0".to_string(),
            "r:v1".into(),
            "r:v2".into(),
            "r:v3".into(),
        ];
        assert!(prune_plan("r", &ts, &pats, 10).is_empty());
        // Retention 5: countable = v3b + v4..v11 (9 tags) -> drop 4 oldest.
        ts.push(("v3b".into(), meta(0, 99)));
        let drops = prune_plan("r", &ts, &pats, 5);
        assert_eq!(drops, vec!["v3b", "v4", "v5", "v6"]);
    }

    #[test]
    fn deterministic_ties() {
        let ts = vec![
            ("b".to_string(), meta(5, 5)),
            ("a".to_string(), meta(5, 5)),
            ("c".to_string(), meta(5, 5)),
        ];
        assert_eq!(prune_plan("r", &ts, &[], 2), vec!["a"]);
    }
}
