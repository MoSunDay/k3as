//! Repository-name / tag / digest grammar checks (Docker Registry HTTP
//! API V2 reference grammar). TODO **T6.3**, decision **Q40**.
//!
//! Validation runs BEFORE any filesystem touch: the repository grammar
//! (`[a-z0-9]+((\.|_|__|-+)[a-z0-9]+)*(\/...)*`, <= 255 chars) admits
//! only `[a-z0-9]` path components, which structurally rules out `..`
//! traversal and absolute segments.

/// Repository name: slash-separated components, total length <= 255.
pub fn valid_repo_name(name: &str) -> bool {
    if name.is_empty() || name.len() > 255 || name.starts_with('/') || name.ends_with('/') {
        return false;
    }
    name.split('/').all(valid_component)
}

fn valid_component(c: &str) -> bool {
    if c.is_empty() || c.len() > 128 {
        return false;
    }
    let alnum = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    let b = c.as_bytes();
    if !alnum(b[0]) || !alnum(b[b.len() - 1]) {
        return false;
    }
    let mut i = 0;
    while i < b.len() {
        if alnum(b[i]) {
            i += 1;
            continue;
        }
        // Separator run between alnum runs: `.` | `_` | `__` | `-`+ .
        let run: &[u8] = {
            let start = i;
            while i < b.len() && !alnum(b[i]) {
                i += 1;
            }
            &b[start..i]
        };
        let dashes = run.iter().all(|&x| x == b'-');
        if !(run == b"." || run == b"_" || run == b"__" || dashes) {
            return false;
        }
    }
    true
}

/// Tag: `[a-zA-Z0-9_][a-zA-Z0-9._-]{0,127}`.
pub fn valid_tag(tag: &str) -> bool {
    let b = tag.as_bytes();
    if b.is_empty() || b.len() > 128 {
        return false;
    }
    let body_ok = |x: u8| x.is_ascii_alphanumeric() || x == b'.' || x == b'_' || x == b'-';
    let head_ok = |x: u8| x.is_ascii_alphanumeric() || x == b'_';
    head_ok(b[0]) && b[1..].iter().all(|&x| body_ok(x))
}

/// Digest: sha256 only, `sha256:` + 64 lowercase hex chars.
pub fn valid_digest(digest: &str) -> bool {
    match digest.strip_prefix("sha256:") {
        Some(hex) => {
            hex.len() == 64
                && hex
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_names() {
        for ok in [
            "a",
            "foo/bar",
            "foo.bar/baz_qux",
            "a__b/c-d",
            "x/y/z",
            "library/ubuntu",
        ] {
            assert!(valid_repo_name(ok), "{ok}");
        }
        for bad in [
            "",
            "/a",
            "a/",
            "Foo",
            "foo//bar",
            "..",
            "a/../b",
            "-lead",
            "trail-",
            "a/.",
            "a/...b",
            "a/-",
            &"a".repeat(256),
        ] {
            assert!(!valid_repo_name(bad), "{bad}");
        }
        // `x__y` is legal per the reference grammar (double underscore).
        assert!(valid_repo_name("x__y"));
    }

    #[test]
    fn tags() {
        for ok in ["latest", "1.0", "v1.0-alpha_beta", "_x", "A-b.2"] {
            assert!(valid_tag(ok), "{ok}");
        }
        for bad in ["", ".hidden", "-x", "x!", &"t".repeat(129), "a b"] {
            assert!(!valid_tag(bad), "{bad}");
        }
    }

    #[test]
    fn digests() {
        let good = format!("sha256:{}", "a".repeat(64));
        assert!(valid_digest(&good));
        assert!(!valid_digest("sha256:abc"));
        assert!(!valid_digest(&format!("sha256:{}", "A".repeat(64))));
        assert!(!valid_digest(&format!("sha512:{}", "a".repeat(64))));
    }
}
