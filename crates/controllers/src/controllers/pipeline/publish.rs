//! Registry address / repository splitting + log-tail bounding for the
//! ServicePipeline controller (TODO **T8.3**, decision **Q56**).
//!
//! `spec.image.repository` may or may not carry the registry authority:
//! `192.168.31.57:15000/k3as/code-grapher` is host-full, while
//! `k3as/code-grapher` is host-less and resolves against the configured
//! [`super::PipelineConfig::registry_addr`]. The docker registry push API
//! needs the authority and the repository separately, so [`registry_and_repo`]
//! makes that split a pure, unit-tested decision.
//!
//! [`tail_chars`] is the char-boundary-safe tail used by the status writer:
//! docker output is unbounded, a status object is not.

/// Marker prefixed to a bounded log tail when content was dropped.
pub(crate) const TRUNCATED_MARKER: &str = "...(truncated)\n";

/// Split `repository` into `(registry_addr, repo)`.
///
/// * `http://` / `https://` schemes are stripped (v1 pushes are plain HTTP
///   to the embedded registry, Q40; a scheme in spec is just noise).
/// * The first path segment is the authority when it contains `:` or `.`
///   or is `localhost` / `::1` (docker's own "is this a registry?"
///   heuristic); the remainder is the repository.
/// * Otherwise `default_addr` hosts the whole (trimmed) repository string.
/// * An empty repository yields `(default_addr, "")`.
pub fn registry_and_repo(repository: &str, default_addr: &str) -> (String, String) {
    let trimmed = repository.trim();
    let stripped = trimmed
        .strip_prefix("http://")
        .or_else(|| trimmed.strip_prefix("https://"))
        .unwrap_or(trimmed)
        .trim_start_matches('/');
    if stripped.is_empty() {
        return (default_addr.to_string(), String::new());
    }
    match stripped.split_once('/') {
        Some((first, rest)) if !rest.trim_matches('/').is_empty() && is_authority(first) => {
            (first.to_string(), rest.trim_matches('/').to_string())
        }
        _ => (
            default_addr.to_string(),
            stripped.trim_end_matches('/').to_string(),
        ),
    }
}

/// Docker's "first segment names a registry" heuristic.
fn is_authority(segment: &str) -> bool {
    segment.contains(':') || segment.contains('.') || segment == "localhost" || segment == "::1"
}

/// The last `max` characters of `text` (char-boundary safe; no marker).
pub fn tail_chars(text: &str, max: usize) -> String {
    let total = text.chars().count();
    if total <= max {
        return text.to_string();
    }
    text.chars().skip(total - max).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEFAULT: &str = "127.0.0.1:15000";

    #[test]
    fn host_full_repository_keeps_its_authority() {
        let (addr, repo) = registry_and_repo("192.168.31.57:15000/k3as/code-grapher", DEFAULT);
        assert_eq!(addr, "192.168.31.57:15000");
        assert_eq!(repo, "k3as/code-grapher");
    }

    #[test]
    fn host_less_repository_uses_the_default_registry() {
        let (addr, repo) = registry_and_repo("k3as/code-grapher", DEFAULT);
        assert_eq!(addr, DEFAULT);
        assert_eq!(repo, "k3as/code-grapher");
        // A dotted host WITHOUT a port is still an authority.
        let (addr, repo) = registry_and_repo("registry.example.com/team/app", DEFAULT);
        assert_eq!(addr, "registry.example.com");
        assert_eq!(repo, "team/app");
        // `localhost` is an authority even without a dot or port.
        let (addr, repo) = registry_and_repo("localhost/app", DEFAULT);
        assert_eq!(addr, "localhost");
        assert_eq!(repo, "app");
    }

    #[test]
    fn schemes_are_stripped_before_the_split() {
        for scheme in ["http://", "https://"] {
            let full = format!("{scheme}reg.example.com:5000/team/app");
            let (addr, repo) = registry_and_repo(&full, DEFAULT);
            assert_eq!(addr, "reg.example.com:5000");
            assert_eq!(repo, "team/app");
        }
    }

    #[test]
    fn degenerate_repositories_fall_back_to_the_default() {
        assert_eq!(
            registry_and_repo("", DEFAULT),
            (DEFAULT.to_string(), String::new())
        );
        assert_eq!(
            registry_and_repo("   ", DEFAULT),
            (DEFAULT.to_string(), String::new())
        );
        // Authority with no repository path: keep the string whole.
        assert_eq!(
            registry_and_repo("localhost:5000", DEFAULT),
            (DEFAULT.to_string(), "localhost:5000".to_string())
        );
        // Trailing slashes are trimmed.
        assert_eq!(
            registry_and_repo("team/app/", DEFAULT),
            (DEFAULT.to_string(), "team/app".to_string())
        );
    }

    #[test]
    fn tail_chars_is_bounded_and_char_boundary_safe() {
        assert_eq!(tail_chars("abcdef", 3), "def");
        assert_eq!(tail_chars("abc", 10), "abc");
        assert_eq!(tail_chars("abc", 0), "");
        // Multi-byte: never split a code point.
        assert_eq!(tail_chars("a\u{e9}bc", 2), "bc");
        assert_eq!(tail_chars("a\u{e9}bc", 3), "\u{e9}bc");
    }
}
