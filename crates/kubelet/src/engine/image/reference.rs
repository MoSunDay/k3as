//! T4.13 image store (Q53/Q54): image reference parsing — the docker
//! reference grammar subset the pull chain speaks. Pure functions only:
//! parsing, canonicalization (`host/repo:tag` / `host/repo@digest`) and
//! host[:port] splitting.

/// A parsed image reference: `host` is the EFFECTIVE registry (the host the
/// reference itself carries, or the configured default), `tag`/`digest` the
/// selector (exactly one is `Some` after parsing).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedRef {
    pub host: String,
    pub repo: String,
    pub tag: Option<String>,
    pub digest: Option<String>,
}

/// `sha256:` + 64 lowercase hex only (docker's v2 rule).
fn valid_digest(d: &str) -> bool {
    match d.strip_prefix("sha256:") {
        Some(hex) => {
            hex.len() == 64
                && hex
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        }
        None => false,
    }
}

/// Parse an image reference against a default registry host.
///
/// Host detection follows the docker rule, kept simple and documented: the
/// FIRST path component is a registry host ONLY when it contains `.` or `:`
/// or is `localhost` — so `busybox` stays repo-relative (default host)
/// while `localhost:5000/a/b` splits its host off. `@sha256:` wins over
/// `:tag` (the tag is dropped); no selector at all means `:latest`. Empty
/// repositories, uppercase repo characters, malformed digest suffixes and
/// non-numeric registry ports are rejected.
pub fn parse_ref(image: &str, default_host: &str) -> Result<ParsedRef, String> {
    let s = image.trim();
    if s.is_empty() {
        return Err("empty image reference".to_string());
    }
    let (left, digest) = match s.split_once('@') {
        Some((l, d)) => {
            if !valid_digest(d) {
                return Err(format!("invalid digest suffix @{d} in {image:?}"));
            }
            (l, Some(d.to_string()))
        }
        None => (s, None),
    };
    let mut host = default_host.to_string();
    let mut rest = left;
    if let Some((first, remainder)) = left.split_once('/') {
        if first.contains('.') || first.contains(':') || first == "localhost" {
            if let Some((_, port)) = first.rsplit_once(':') {
                port.parse::<u16>()
                    .map_err(|_| format!("invalid registry port in {image:?}"))?;
            }
            if remainder.is_empty() {
                return Err(format!(
                    "image reference {image:?} has no repository after the host"
                ));
            }
            host = first.to_string();
            rest = remainder;
        }
    }
    let (repo, tag) = match rest.rsplit_once(':') {
        Some((r, t)) if !t.contains('/') && !t.is_empty() => (r.to_string(), Some(t.to_string())),
        Some((_, t)) => return Err(format!("invalid tag {t:?} in {image:?}")),
        None => (rest.to_string(), None),
    };
    if repo.is_empty() {
        return Err(format!("image reference {image:?} has an empty repository"));
    }
    if repo.chars().any(|c| c.is_ascii_uppercase()) {
        return Err(format!(
            "repository {repo:?} must be lowercase (docker v2 rule)"
        ));
    }
    // A digest selector wins; tag refs default to `latest`.
    let tag = if digest.is_some() {
        None
    } else {
        tag.or_else(|| Some("latest".to_string()))
    };
    Ok(ParsedRef {
        host,
        repo,
        tag,
        digest,
    })
}

/// Canonical reference string: `host/repo:tag` or `host/repo@digest` — the
/// single-flight key AND the ref-table identity.
pub fn normalized(pr: &ParsedRef) -> String {
    match (&pr.tag, &pr.digest) {
        (Some(t), None) => format!("{}/{}:{}", pr.host, pr.repo, t),
        _ => format!(
            "{}/{}@{}",
            pr.host,
            pr.repo,
            pr.digest.clone().unwrap_or_default()
        ),
    }
}

/// Split `host[:port]`; a missing port defaults to 80 (plain HTTP only,
/// Q21 — TLS arrives with T1.3).
pub fn host_port(host: &str) -> Result<(String, u16), String> {
    match host.rsplit_once(':') {
        Some((h, p)) => {
            let port = p
                .parse::<u16>()
                .map_err(|_| format!("invalid registry port in host {host:?}"))?;
            Ok((h.to_string(), port))
        }
        None => Ok((host.to_string(), 80)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DH: &str = "127.0.0.1:15000";

    #[test]
    fn parse_ref_matrix() {
        let p = parse_ref("busybox", DH).unwrap();
        assert_eq!(
            p,
            ParsedRef {
                host: DH.into(),
                repo: "busybox".into(),
                tag: Some("latest".into()),
                digest: None,
            }
        );
        let p = parse_ref("busybox:1.36", DH).unwrap();
        assert_eq!(
            (p.repo.as_str(), p.tag.as_deref()),
            ("busybox", Some("1.36"))
        );
        let p = parse_ref("localhost:5000/a/b:v1", DH).unwrap();
        assert_eq!(
            (p.host.as_str(), p.repo.as_str(), p.tag.as_deref()),
            ("localhost:5000", "a/b", Some("v1"))
        );
        let d64 = format!("sha256:{}", "a".repeat(64));
        let p = parse_ref(&format!("reg.io/x/y@{d64}"), DH).unwrap();
        assert_eq!(
            (p.host.as_str(), p.repo.as_str(), p.digest.as_deref()),
            ("reg.io", "x/y", Some(d64.as_str()))
        );
        assert_eq!(p.tag, None, "digest wins over any tag");
        let p = parse_ref(&format!("foo@{d64}"), DH).unwrap();
        assert_eq!((p.host.as_str(), p.repo.as_str()), (DH, "foo"));
        for bad in [
            "",
            "reg.io/",
            "busybox:",
            "Foo/bar:v1",
            "foo@sha256:short",
            "foo@md5:abcd",
            "localhost:notaport/x:v1",
        ] {
            assert!(parse_ref(bad, DH).is_err(), "{bad:?} must be rejected");
        }
    }

    #[test]
    fn normalized_spells_key_and_host_port_splits() {
        let p = parse_ref("busybox", DH).unwrap();
        assert_eq!(normalized(&p), "127.0.0.1:15000/busybox:latest");
        let p = parse_ref("reg.io/x/y:v2", DH).unwrap();
        assert_eq!(normalized(&p), "reg.io/x/y:v2");
        assert_eq!(host_port("reg.io").unwrap(), ("reg.io".to_string(), 80));
        assert_eq!(
            host_port("127.0.0.1:15000").unwrap(),
            ("127.0.0.1".to_string(), 15000)
        );
        assert!(host_port("h:x").is_err());
    }
}
