//! Pinned upstream-artifact manifest (T0.2 acquire, Q6).
//!
//! `vendor/versions.toml` lists the subprocess binaries init-pro bundles
//! (containerd, runc, CNI multicall — all Apache-2.0 per Q7). Each entry is
//! content-pinned by the upstream release SHA-256; the acquire step (Q6)
//! downloads, verifies, and stages into `vendor/bin/`.

#![forbid(unsafe_code)]

use serde::Deserialize;

/// Q7 license allow-list: any artifact outside this set fails the build.
pub const LICENSE_ALLOW: &[&str] = &["Apache-2.0", "BSD-2-Clause", "BSD-3-Clause", "MIT", "ISC"];

/// One pinned upstream artifact.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Artifact {
    /// Human name, e.g. `containerd`.
    pub name: String,
    /// Upstream release version, e.g. `1.7.20`.
    pub version: String,
    /// SPDX license id (consumed by the Q7 allow-list gate).
    pub license: String,
    /// Download URL (GitHub release asset).
    pub url: String,
    /// Hex SHA-256 of the downloaded artifact (the pin).
    pub sha256: String,
    /// `tar` (extract) or `bin` (copy a single binary).
    #[serde(rename = "type")]
    pub kind: Kind,
    /// Subdir under `vendor/bin/` where files land. Default `.` (root).
    #[serde(default = "default_into")]
    pub into: String,
    /// tar `--strip-components`. Default 0.
    #[serde(default)]
    pub strip: u32,
    /// For `type = "bin"`: target filename. Defaults to `name`.
    #[serde(default)]
    pub install_as: Option<String>,
}

/// How an artifact is unpacked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Tar,
    Bin,
}

fn default_into() -> String {
    ".".to_string()
}

impl Artifact {
    /// Filename used in the download cache (`vendor/cache/`).
    pub fn cache_name(&self) -> String {
        // Last URL path segment; falls back to `name` if the URL is odd.
        self.url
            .rsplit('/')
            .next()
            .filter(|s| !s.is_empty())
            .unwrap_or(&self.name)
            .to_string()
    }

    /// Target filename for a `bin` artifact.
    pub fn bin_name(&self) -> &str {
        self.install_as.as_deref().unwrap_or(&self.name)
    }

    /// Validates the parsed fields. Returns `Err(msg)` on the first problem.
    pub fn validate(&self) -> Result<(), String> {
        if self.name.trim().is_empty() {
            return Err("artifact has empty `name`".into());
        }
        if !is_hex64(&self.sha256) {
            return Err(format!("artifact `{}` has invalid `sha256`", self.name));
        }
        if !self.url.starts_with("https://") {
            return Err(format!("artifact `{}` url must be https://", self.name));
        }
        if !LICENSE_ALLOW.contains(&self.license.as_str()) {
            // Non-cleared licenses must fail the build (Q7 gate).
            return Err(format!(
                "artifact `{}` license `{}` is not on the allow-list {:?}",
                self.name, self.license, LICENSE_ALLOW
            ));
        }
        Ok(())
    }
}

fn is_hex64(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Parse a TOML manifest string into validated artifacts.
pub fn parse(src: &str) -> Result<Vec<Artifact>, ParseError> {
    #[derive(Deserialize)]
    struct Raw {
        #[serde(default)]
        artifact: Vec<Artifact>,
    }
    let raw: Raw = toml::from_str(src).map_err(ParseError::Toml)?;
    if raw.artifact.is_empty() {
        return Err(ParseError::Empty);
    }
    for a in &raw.artifact {
        a.validate().map_err(ParseError::Invalid)?;
    }
    Ok(raw.artifact)
}

/// Manifest parse/validation error.
#[derive(Debug)]
pub enum ParseError {
    Toml(toml::de::Error),
    Invalid(String),
    Empty,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseError::Toml(e) => write!(f, "versions.toml parse error: {e}"),
            ParseError::Invalid(m) => write!(f, "versions.toml invalid: {m}"),
            ParseError::Empty => write!(f, "versions.toml has no [[artifact]] entries"),
        }
    }
}
impl std::error::Error for ParseError {}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = r#"
[[artifact]]
name = "runc"
version = "1.1.13"
license = "Apache-2.0"
url = "https://example.invalid/runc.amd64"
sha256 = "bcfc299c1ab255e9d045ffaf2e324c0abaf58f599831a7c2c4a80b33f795de94"
type = "bin"
install_as = "runc"

[[artifact]]
name = "cni-plugins"
version = "1.5.1"
license = "Apache-2.0"
url = "https://example.invalid/cni.tgz"
sha256 = "77baa2f669980a82255ffa2f2717de823992480271ee778aa51a9c60ae89ff9b"
type = "tar"
into = "aux"
"#;

    #[test]
    fn parses_valid_manifest() {
        let arts = parse(GOOD).unwrap();
        assert_eq!(arts.len(), 2);
        assert_eq!(arts[0].name, "runc");
        assert_eq!(arts[0].kind, Kind::Bin);
        assert_eq!(arts[0].bin_name(), "runc");
        assert_eq!(arts[1].kind, Kind::Tar);
        assert_eq!(arts[1].into, "aux");
        assert_eq!(arts[1].strip, 0);
        assert_eq!(arts[1].cache_name(), "cni.tgz");
    }

    #[test]
    fn rejects_empty_manifest() {
        let err = parse("# nothing").unwrap_err();
        assert!(matches!(err, ParseError::Empty), "{err:?}");
    }

    #[test]
    fn rejects_bad_sha256() {
        let bad = GOOD.replace(
            "bcfc299c1ab255e9d045ffaf2e324c0abaf58f599831a7c2c4a80b33f795de94",
            "tooshort",
        );
        assert!(parse(&bad).is_err());
    }

    #[test]
    fn rejects_non_https_url() {
        let bad = GOOD.replace("https://example.invalid/runc.amd64", "http://insecure/x");
        assert!(parse(&bad).is_err());
    }

    #[test]
    fn rejects_uncleared_license() {
        let bad = GOOD.replace("Apache-2.0", "GPL-2.0", );
        let err = parse(&bad).unwrap_err();
        assert!(err.to_string().contains("GPL-2.0"), "{err}");
    }

    #[test]
    fn defaults_into_root() {
        let arts = parse(GOOD).unwrap();
        assert_eq!(arts[0].into, ".", "bin artifact defaults into root");
    }
}
