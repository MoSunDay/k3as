//! Q7 license allow-list gate + SPDX-2.3 SBOM generation (T0.2-B4).
//!
//! Every artifact bundled by `init-pro-vendor` must clear the Q7 license
//! allow-list ([`ALLOWED_LICENSES`]) before it is staged. [`validate`] enforces
//! that gate over a slice of parsed [`Artifact`]s; [`render_spdx`] emits a
//! SPDX-2.3 JSON document describing the cleared set.
//!
//! Pure functions only — no I/O, no global state. JSON is built by hand (no
//! serde on the output path) so the SBOM stage adds no extra dependency.

#![forbid(unsafe_code)]

use crate::manifest::Artifact;

/// Q7 license allow-list — licenses cleared for inclusion in init-pro v1.
pub const ALLOWED_LICENSES: &[&str] = &[
    "Apache-2.0",
    "BSD-2-Clause",
    "BSD-3-Clause",
    "MIT",
    "ISC",
];

/// A license failed the Q7 allow-list gate.
///
/// Carries the offending SPDX id and the name of the artifact that declared it
/// so callers can report a precise, actionable error.
#[derive(Debug)]
pub struct SbomError {
    /// The offending SPDX license id (exactly as declared on the artifact).
    pub license: String,
    /// Name of the artifact that declared it.
    pub artifact: String,
}

impl std::fmt::Display for SbomError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "artifact `{}` declares license `{}` which is not on the Q7 allow-list {:?} \
             — only cleared licenses may be bundled into init-pro",
            self.artifact, self.license, ALLOWED_LICENSES,
        )
    }
}

impl std::error::Error for SbomError {}

/// Case-sensitive exact match against [`ALLOWED_LICENSES`].
pub fn is_allowed(license: &str) -> bool {
    ALLOWED_LICENSES.contains(&license)
}

/// Validate every artifact's `license` against [`ALLOWED_LICENSES`].
///
/// Returns `Err` on the first non-cleared artifact, preserving manifest order
/// so the reported failure points at the earliest offender.
pub fn validate(artifacts: &[Artifact]) -> Result<(), SbomError> {
    for a in artifacts {
        if !is_allowed(&a.license) {
            return Err(SbomError {
                license: a.license.clone(),
                artifact: a.name.clone(),
            });
        }
    }
    Ok(())
}

/// Escape a string for safe embedding inside a JSON `"..."` literal.
///
/// Handles the mandatory JSON escapes (`"`, `\`) plus control characters
/// (`\n`, `\r`, `\t`, `\b`, `\f`, and any other control codepoint).
fn json_escape(s: &str) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if c.is_control() => {
                let _ = write!(out, "\\u{:04x}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out
}

/// Render the JSON object for a single SPDX package (no surrounding list,
/// no trailing comma).
fn package_object(a: &Artifact) -> String {
    let name = json_escape(&a.name);
    let version = json_escape(&a.version);
    let license = json_escape(&a.license);
    let url = json_escape(&a.url);
    let sha = json_escape(&a.sha256);
    format!(
        concat!(
            "    {{\n",
            "      \"name\": \"{name}\",\n",
            "      \"SPDXID\": \"SPDXRef-Package-{name}\",\n",
            "      \"versionInfo\": \"{version}\",\n",
            "      \"licenseConcluded\": \"{license}\",\n",
            "      \"licenseDeclared\": \"{license}\",\n",
            "      \"downloadLocation\": \"{url}\",\n",
            "      \"filesAnalyzed\": false,\n",
            "      \"checksums\": [\n",
            "        {{\n",
            "          \"algorithm\": \"SHA256\",\n",
            "          \"checksumValue\": \"{sha}\"\n",
            "        }}\n",
            "      ]\n",
            "    }}",
        ),
        name = name,
        version = version,
        license = license,
        url = url,
        sha = sha,
    )
}

/// Render a complete SPDX-2.3 JSON document for `artifacts`.
///
/// `created` is an ISO-8601 timestamp (e.g. `"2026-07-26T00:00:00Z"`); it is
/// embedded both in the document namespace and in `creationInfo.created`.
/// All interpolated values are JSON-escaped — the output is safe text.
pub fn render_spdx(artifacts: &[Artifact], created: &str) -> String {
    let created = json_escape(created);
    let packages = artifacts
        .iter()
        .map(package_object)
        .collect::<Vec<_>>()
        .join(",\n");

    format!(
        concat!(
            "{{\n",
            "  \"spdxVersion\": \"SPDX-2.3\",\n",
            "  \"dataLicense\": \"CC0-1.0\",\n",
            "  \"SPDXID\": \"SPDXRef-DOCUMENT\",\n",
            "  \"name\": \"init-pro-vendor\",\n",
            "  \"documentNamespace\": \"https://init-pro.dev/spdx/{created}\",\n",
            "  \"creationInfo\": {{\n",
            "    \"creators\": [\"Tool: init-pro-vendor\"],\n",
            "    \"created\": \"{created}\"\n",
            "  }},\n",
            "  \"packages\": [\n{packages}\n  ]\n",
            "}}\n",
        ),
        created = created,
        packages = packages,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::Kind;

    /// Minimal artifact for license/SBOM tests (legal Apache-2.0 by default-ish).
    fn artifact(name: &str, license: &str) -> Artifact {
        Artifact {
            name: name.to_string(),
            version: "1.0.0".to_string(),
            license: license.to_string(),
            url: format!("https://example.invalid/{name}.tgz"),
            sha256: "a".repeat(64),
            kind: Kind::Bin,
            into: ".".to_string(),
            strip: 0,
            install_as: None,
        }
    }

    #[test]
    fn is_allowed_accepts_every_allow_listed_license() {
        for &lic in ALLOWED_LICENSES {
            assert!(is_allowed(lic), "{lic} should be allowed");
        }
    }

    #[test]
    fn is_allowed_rejects_disallowed_licenses() {
        for &lic in &["GPL-3.0", "AGPL-3.0", ""] {
            assert!(!is_allowed(lic), "{lic:?} should NOT be allowed");
        }
    }

    #[test]
    fn is_allowed_is_case_sensitive_exact_match() {
        assert!(is_allowed("MIT"));
        assert!(!is_allowed("mit"));
        assert!(!is_allowed("apache-2.0"));
        assert!(!is_allowed("MIT "));
        assert!(!is_allowed(" MIT"));
    }

    #[test]
    fn validate_passes_when_all_artifacts_are_cleared() {
        let arts = [
            artifact("containerd", "Apache-2.0"),
            artifact("runc", "Apache-2.0"),
            artifact("cni", "MIT"),
        ];
        assert!(validate(&arts).is_ok());
    }

    #[test]
    fn validate_fails_on_first_non_cleared_artifact() {
        let arts = [
            artifact("containerd", "Apache-2.0"),
            artifact("forbidden-tool", "GPL-2.0"),
            artifact("runc", "MIT"),
        ];
        let err = validate(&arts).unwrap_err();
        assert_eq!(err.artifact, "forbidden-tool");
        assert_eq!(err.license, "GPL-2.0");
        let msg = err.to_string();
        assert!(msg.contains("GPL-2.0"), "message names license: {msg}");
        assert!(msg.contains("forbidden-tool"), "message names artifact: {msg}");
        assert!(msg.contains("Apache-2.0"), "message lists what is allowed: {msg}");
    }

    #[test]
    fn validate_passes_on_empty_set() {
        assert!(validate(&[]).is_ok());
    }

    #[test]
    fn render_spdx_contains_required_top_level_fields() {
        let arts = [artifact("containerd", "Apache-2.0")];
        let doc = render_spdx(&arts, "2026-07-26T00:00:00Z");
        assert!(doc.contains("\"spdxVersion\": \"SPDX-2.3\""));
        assert!(doc.contains("\"dataLicense\": \"CC0-1.0\""));
        assert!(doc.contains("\"SPDXID\": \"SPDXRef-DOCUMENT\""));
        assert!(doc.contains("\"name\": \"init-pro-vendor\""));
        assert!(doc.contains("\"documentNamespace\": \"https://init-pro.dev/spdx/2026-07-26T00:00:00Z\""));
        assert!(doc.contains("\"creators\": [\"Tool: init-pro-vendor\"]"));
        assert!(doc.contains("\"created\": \"2026-07-26T00:00:00Z\""));
        assert!(doc.contains("\"packages\":"));
    }

    #[test]
    fn render_spdx_emits_one_package_per_artifact() {
        let arts = [
            artifact("containerd", "Apache-2.0"),
            artifact("runc", "MIT"),
        ];
        let doc = render_spdx(&arts, "2026-07-26T00:00:00Z");
        for a in &arts {
            assert!(doc.contains(&format!("\"name\": \"{}\"", a.name)), "missing name {}", a.name);
            assert!(
                doc.contains(&format!("SPDXRef-Package-{}", a.name)),
                "missing SPDXID for {}",
                a.name
            );
            assert!(doc.contains(&format!("\"versionInfo\": \"{}\"", a.version)));
            assert!(doc.contains(&format!("\"licenseConcluded\": \"{}\"", a.license)));
            assert!(doc.contains(&format!("\"licenseDeclared\": \"{}\"", a.license)));
            assert!(doc.contains(&format!("\"downloadLocation\": \"{}\"", a.url)));
            assert!(doc.contains(&format!("\"checksumValue\": \"{}\"", a.sha256)));
        }
        assert!(doc.contains("\"filesAnalyzed\": false"));
        assert!(doc.contains("\"algorithm\": \"SHA256\""));
    }

    #[test]
    fn render_spdx_empty_artifacts_has_empty_packages_array() {
        let doc = render_spdx(&[], "2026-07-26T00:00:00Z");
        assert!(doc.contains("\"packages\""));
        assert!(!doc.contains("SPDXRef-Package-"));
        // Still a well-formed envelope.
        assert_eq!(doc.matches('{').count(), doc.matches('}').count());
        assert_eq!(doc.matches('"').count() % 2, 0);
    }

    #[test]
    fn render_spdx_is_well_formed_json_envelope() {
        let arts = [
            artifact("containerd", "Apache-2.0"),
            artifact("runc", "MIT"),
            artifact("cni", "ISC"),
        ];
        let doc = render_spdx(&arts, "2026-07-26T00:00:00Z");
        let trimmed = doc.trim();
        assert!(trimmed.starts_with('{'), "must start with `{{`");
        assert!(trimmed.ends_with('}'), "must end with `}}`");
        assert_eq!(
            doc.matches('{').count(),
            doc.matches('}').count(),
            "unbalanced braces",
        );
        assert_eq!(
            doc.matches('[').count(),
            doc.matches(']').count(),
            "unbalanced brackets",
        );
        assert_eq!(doc.matches('"').count() % 2, 0, "odd number of double-quotes");
    }

    #[test]
    fn render_spdx_escapes_special_chars() {
        // A version carrying JSON-significant characters must be escaped, not
        // emitted raw (an unescaped `"` would corrupt the structure).
        let mut a = artifact("weird-name", "MIT");
        a.version = "1.0\"back\\slash".to_string();
        let doc = render_spdx(&[a], "2026-07-26T00:00:00Z");
        // The raw value must never appear verbatim ...
        assert!(
            !doc.contains("\"versionInfo\": \"1.0\"back\\slash\""),
            "unescaped value leaked into JSON",
        );
        // ... and the escaped form must be present.
        assert!(
            doc.contains("1.0\\\"back\\\\slash"),
            "value not escaped correctly",
        );
        // Structural balance preserved (braces/brackets are used only structurally).
        assert_eq!(doc.matches('{').count(), doc.matches('}').count());
        assert_eq!(doc.matches('[').count(), doc.matches(']').count());
    }

    #[test]
    fn json_escape_handles_known_sequences() {
        assert_eq!(json_escape("plain"), "plain");
        assert_eq!(json_escape(""), "");
        assert_eq!(json_escape("a\"b"), "a\\\"b");
        assert_eq!(json_escape("a\\b"), "a\\\\b");
        assert_eq!(json_escape("line\nbreak"), "line\\nbreak");
        assert_eq!(json_escape("car\rret"), "car\\rret");
        assert_eq!(json_escape("tab\tstop"), "tab\\tstop");
        assert_eq!(json_escape("unit\x07bell"), "unit\\u0007bell");
    }
}
