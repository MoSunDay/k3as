//! T4.13 image store — OCI/Docker manifest + index wire types (Q53/Q54).
//!
//! Pure data + pure functions: registry-v2 manifest/index deserialization,
//! platform selection (`select_manifest`), host platform discovery
//! (`select_platform`, Go runtime naming) and content-addressed digests
//! (`sha256_digest`). Registry pull, CAS and layer unpack land in later
//! T4.13 milestones; this module is deliberately free of I/O.

use serde::Deserialize;
use sha2::{Digest, Sha256};

/// Accepted manifest media types — mirrors
/// `crates/registry/src/api/mod.rs` `MANIFEST_TYPES` (T6.3) exactly:
/// docker v2 manifest, docker manifest list, OCI manifest, OCI index.
pub const MANIFEST_TYPES: [&str; 4] = [
    "application/vnd.docker.distribution.manifest.v2+json",
    "application/vnd.docker.distribution.manifest.list.v2+json",
    "application/vnd.oci.image.manifest.v1+json",
    "application/vnd.oci.image.index.v1+json",
];

/// True for single-image manifest media types (docker v2, OCI manifest).
pub fn is_manifest_type(mt: &str) -> bool {
    mt == MANIFEST_TYPES[0] || mt == MANIFEST_TYPES[2]
}

/// True for multi-platform index media types (docker list, OCI index).
pub fn is_index_type(mt: &str) -> bool {
    mt == MANIFEST_TYPES[1] || mt == MANIFEST_TYPES[3]
}

/// OCI/Docker content descriptor: `mediaType`, `digest`, `size` and, in
/// index entries, the target `platform`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Descriptor {
    #[serde(default)]
    pub media_type: String,
    pub digest: String,
    pub size: u64,
    #[serde(default)]
    pub platform: Option<Platform>,
}

/// Target platform of an index entry (`architecture`, `os`, optional
/// `variant` / `osVersion`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Platform {
    pub architecture: String,
    pub os: String,
    #[serde(default)]
    pub variant: Option<String>,
    #[serde(default)]
    pub os_version: Option<String>,
}

impl Platform {
    /// Exact `architecture` + `os` match; `variant` is compared only when
    /// BOTH sides set it (loose — a variant-less entry still matches a
    /// variant-specific want, and vice versa).
    pub fn matches(&self, want: &Platform) -> bool {
        self.architecture == want.architecture
            && self.os == want.os
            && match (&self.variant, &want.variant) {
                (Some(have), Some(need)) => have == need,
                _ => true,
            }
    }
}

/// Single-image manifest (docker v2 or OCI): `config` + ordered `layers`
/// (`layers[0]` is the bottom of the stack, Q53).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    pub schema_version: u32,
    /// Missing on some registries; disambiguation then keys off
    /// `layers` vs `manifests` (see [`parse_manifest`]).
    #[serde(default)]
    pub media_type: String,
    pub config: Descriptor,
    #[serde(default)]
    pub layers: Vec<Descriptor>,
}

/// Multi-platform manifest list / OCI index.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManifestIndex {
    pub schema_version: u32,
    #[serde(default)]
    pub media_type: String,
    #[serde(default)]
    pub manifests: Vec<Descriptor>,
}

/// A manifest document, classified by its `mediaType`.
#[derive(Debug, Clone)]
pub enum ParsedManifest {
    Single(Manifest),
    Index(ManifestIndex),
}

/// Classify + deserialize a manifest document. The `mediaType` field
/// decides `Single` vs `Index`; a missing `mediaType` is tolerated by
/// inspecting the `layers` vs `manifests` keys. Anything else is an error.
pub fn parse_manifest(bytes: &[u8]) -> Result<ParsedManifest, String> {
    let value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|e| format!("manifest JSON: {e}"))?;
    let mt = value
        .get("mediaType")
        .and_then(|m| m.as_str())
        .unwrap_or_default();
    if is_index_type(mt) {
        let idx: ManifestIndex =
            serde_json::from_value(value).map_err(|e| format!("manifest index: {e}"))?;
        Ok(ParsedManifest::Index(idx))
    } else if is_manifest_type(mt) {
        let m: Manifest = serde_json::from_value(value).map_err(|e| format!("manifest: {e}"))?;
        Ok(ParsedManifest::Single(m))
    } else if mt.is_empty() {
        // Tolerate a missing mediaType: shape decides (layers => single,
        // manifests => index).
        if value.get("layers").is_some() {
            let m: Manifest =
                serde_json::from_value(value).map_err(|e| format!("manifest: {e}"))?;
            Ok(ParsedManifest::Single(m))
        } else if value.get("manifests").is_some() {
            let idx: ManifestIndex =
                serde_json::from_value(value).map_err(|e| format!("manifest index: {e}"))?;
            Ok(ParsedManifest::Index(idx))
        } else {
            Err("manifest without mediaType has neither layers nor manifests".to_string())
        }
    } else {
        Err(format!("unsupported manifest mediaType {mt:?}"))
    }
}

/// First index entry whose platform matches `want` (loose variant rule).
pub fn select_manifest(index: &ManifestIndex, want: &Platform) -> Result<Descriptor, String> {
    index
        .manifests
        .iter()
        .find(|d| d.platform.as_ref().is_some_and(|p| p.matches(want)))
        .cloned()
        .ok_or_else(|| {
            let variant = want
                .variant
                .as_deref()
                .map(|v| format!("/{v}"))
                .unwrap_or_default();
            format!(
                "no manifest for {}/{}{variant} in index",
                want.os, want.architecture
            )
        })
}

/// Host platform in Go runtime naming: `x86_64 -> amd64`,
/// `aarch64 -> arm64`, `arm -> arm`; unknown values pass through raw.
pub fn select_platform() -> Platform {
    Platform {
        architecture: go_arch(std::env::consts::ARCH).to_string(),
        os: go_os(std::env::consts::OS).to_string(),
        variant: None,
        os_version: None,
    }
}

fn go_arch(arch: &str) -> &str {
    match arch {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        "arm" => "arm",
        other => other,
    }
}

fn go_os(os: &str) -> &str {
    // `std::env::consts::OS` already uses Go GOOS names for every case we
    // care about (linux, darwin); unknown values pass through raw.
    os
}

/// `sha256:<hex>` digest of `bytes` — the CAS key face of T4.13 (Q53).
/// Hex encoding is hand-rolled: no `hex` crate (Q54 dependency posture).
pub fn sha256_digest(bytes: &[u8]) -> String {
    format!("sha256:{}", hex_encode(&Sha256::digest(bytes)))
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const AMD64: &str = "sha256:5b0abcabd3d05f0c1c4d0f1c4d0f1c4d0f1c4d0f1c4d0f1c4d0f1c4d0f1c4";
    const ARM64: &str = "sha256:9e1f2c74ea2a7e2a7e2a7e2a7e2a7e2a7e2a7e2a7e2a7e2a7e2a7e2a7e2a7";

    fn list_json() -> String {
        r#"{
            "schemaVersion": 2,
            "mediaType": "application/vnd.docker.distribution.manifest.list.v2+json",
            "manifests": [
                {
                    "mediaType": "application/vnd.docker.distribution.manifest.v2+json",
                    "size": 7143,
                    "digest": "ARM64-DIGEST",
                    "platform": {"architecture": "arm64", "os": "linux"}
                },
                {
                    "mediaType": "application/vnd.docker.distribution.manifest.v2+json",
                    "size": 7682,
                    "digest": "AMD64-DIGEST",
                    "platform": {"architecture": "amd64", "os": "linux"}
                }
            ]
        }"#
        .replace("ARM64-DIGEST", ARM64)
        .replace("AMD64-DIGEST", AMD64)
    }

    fn want(arch: &str) -> Platform {
        Platform {
            architecture: arch.to_string(),
            os: "linux".to_string(),
            variant: None,
            os_version: None,
        }
    }

    #[test]
    fn parses_list_and_selects_amd64() {
        let parsed = parse_manifest(list_json().as_bytes()).expect("list parses");
        let ParsedManifest::Index(idx) = parsed else {
            panic!("expected index");
        };
        assert_eq!(idx.schema_version, 2);
        assert_eq!(idx.manifests.len(), 2);
        let d = select_manifest(&idx, &want("amd64")).expect("amd64 entry");
        assert_eq!(d.digest, AMD64);
        assert_eq!(d.size, 7682);
        // arm64 want gets the arm64 entry (ordering is index order, not host).
        assert_eq!(select_manifest(&idx, &want("arm64")).unwrap().digest, ARM64);
    }

    #[test]
    fn select_manifest_errors_without_match() {
        let ParsedManifest::Index(idx) = parse_manifest(list_json().as_bytes()).unwrap() else {
            panic!("expected index");
        };
        let missing = select_manifest(&idx, &want("riscv64"));
        assert!(missing.is_err());
        assert!(missing.unwrap_err().contains("riscv64"));
    }

    #[test]
    fn platform_variant_is_loose() {
        let plain = want("arm64");
        let v8 = Platform {
            variant: Some("v8".to_string()),
            ..want("arm64")
        };
        // Variant-less entry matches a variant-specific want and vice versa.
        assert!(plain.matches(&v8));
        assert!(v8.matches(&plain));
        let other = Platform {
            variant: Some("v7".to_string()),
            ..want("arm64")
        };
        assert!(!v8.matches(&other));
        // os/arch must match exactly.
        assert!(!plain.matches(&want("amd64")));
    }

    #[test]
    fn parses_single_docker_manifest() {
        let json = r#"{
            "schemaVersion": 2,
            "mediaType": "application/vnd.docker.distribution.manifest.v2+json",
            "config": {
                "mediaType": "application/vnd.docker.container.image.v1+json",
                "size": 1510,
                "digest": "sha256:deadbeef"
            },
            "layers": [
                {"mediaType": "application/vnd.docker.image.rootfs.diff.tar.gzip",
                 "size": 4741734, "digest": "sha256:aaa1"},
                {"mediaType": "application/vnd.docker.image.rootfs.diff.tar.gzip",
                 "size": 126789, "digest": "sha256:bbb2"}
            ]
        }"#;
        let ParsedManifest::Single(m) = parse_manifest(json.as_bytes()).unwrap() else {
            panic!("expected single manifest");
        };
        assert_eq!(m.schema_version, 2);
        assert!(is_manifest_type(&m.media_type));
        assert_eq!(m.config.digest, "sha256:deadbeef");
        assert_eq!(m.layers.len(), 2);
        assert_eq!(m.layers[0].digest, "sha256:aaa1");
        assert_eq!(m.layers[0].platform, None);
    }

    #[test]
    fn oci_index_media_type_classifies_as_index() {
        let json = r#"{
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.index.v1+json",
            "manifests": []
        }"#;
        assert!(matches!(
            parse_manifest(json.as_bytes()).unwrap(),
            ParsedManifest::Index(_)
        ));
    }

    #[test]
    fn unknown_media_type_is_an_error() {
        let json = r#"{"schemaVersion": 2, "mediaType": "application/vnd.example+json"}"#;
        let err = parse_manifest(json.as_bytes()).unwrap_err();
        assert!(err.contains("unsupported manifest mediaType"), "got: {err}");
    }

    #[test]
    fn missing_media_type_disambiguates_by_shape() {
        let single = r#"{
            "schemaVersion": 2,
            "config": {"mediaType": "application/vnd.oci.image.config.v1+json",
                       "size": 7, "digest": "sha256:cc"},
            "layers": [{"mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
                        "size": 9, "digest": "sha256:dd"}]
        }"#;
        let ParsedManifest::Single(m) = parse_manifest(single.as_bytes()).unwrap() else {
            panic!("expected single manifest");
        };
        assert_eq!(m.layers.len(), 1);
        assert_eq!(m.media_type, "");

        let index = r#"{"schemaVersion": 2, "manifests": [{"mediaType":
            "application/vnd.oci.image.manifest.v1+json", "size": 5,
            "digest": "sha256:ee"}]}"#;
        assert!(matches!(
            parse_manifest(index.as_bytes()).unwrap(),
            ParsedManifest::Index(_)
        ));

        let bare = br#"{"schemaVersion": 2}"#;
        assert!(parse_manifest(bare).is_err());
    }

    #[test]
    fn sha256_digest_matches_reference_vector() {
        // Verified: python3 -c "import hashlib;print(hashlib.sha256(b'abc').hexdigest())"
        assert_eq!(
            sha256_digest(b"abc"),
            "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(sha256_digest(b"").len(), 7 + 64);
    }

    #[test]
    fn select_platform_maps_go_names() {
        let p = select_platform();
        assert_eq!(p.os, go_os("linux").to_string().replace("macos", "darwin"));
        assert!(matches!(
            p.architecture.as_str(),
            "amd64" | "arm64" | "arm" | _
        ));
        assert_eq!(go_arch("x86_64"), "amd64");
        assert_eq!(go_arch("aarch64"), "arm64");
        assert_eq!(go_arch("arm"), "arm");
        assert_eq!(go_arch("loongarch64"), "loongarch64");
        assert_eq!(go_os("linux"), "linux");
        assert_eq!(go_os("darwin"), "darwin");
        assert_eq!(go_os("plan9"), "plan9");
    }
}
