//! Docker Registry HTTP API v2 **push** client (TODO **T6.3**, M1 slice;
//! **T8.1** / Q38+Q40).
//!
//! The outbound mirror of the server-side plane in this crate: a
//! dependency-free HTTP/1.1 client (raw `tokio::net::TcpStream`, one
//! connection per request, `Connection: close`, Content-Length framing -
//! see the sibling `http` module) that performs the canonical blob dance
//! docker and containerd use:
//!
//! ```text
//! POST /v2/<name>/blobs/uploads/          -> 202 + Location
//! PATCH <location>                        -> 202 (+ Location)
//! PUT   <location>?digest=sha256%3A<hex>  -> 201
//! PUT   /v2/<name>/manifests/<tag>        -> 201 + Docker-Content-Digest
//! ```
//!
//! The registry answers with RELATIVE `Location` headers, so every
//! location is resolved against the registry authority before use.
//! Plain HTTP, no auth (T8.1, Q38): the M1 image plane pushes into the
//! in-process registry, so `https://` is rejected outright instead of
//! being silently downgraded.

use std::fmt;

use serde_json::json;
use sha2::{Digest as _, Sha256};

use crate::http::{exchange, resolve_location, Reply, Target};

/// Media type of the image config blob (docker schema 2).
pub const CONFIG_MEDIA_TYPE: &str = "application/vnd.docker.container.image.v1+json";
/// Media type of an uncompressed docker layer tar (`docker save` shape).
pub const LAYER_MEDIA_TYPE: &str = "application/vnd.docker.image.rootfs.diff.tar";
/// Media type of a docker v2 schema-2 image manifest.
pub const MANIFEST_MEDIA_TYPE: &str = "application/vnd.docker.distribution.manifest.v2+json";

/// One image layer: media type plus the raw layer tar bytes (already
/// gzipped when the media type says so).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayerBlob {
    pub media_type: String,
    pub data: Vec<u8>,
}

/// The complete payload `push_image` needs: config JSON plus ordered
/// layers (bottom first).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushImage {
    pub config: Vec<u8>,
    pub config_media_type: String,
    pub layers: Vec<LayerBlob>,
}

/// Why a push failed: local transport/framing trouble, an unexpected
/// registry answer (status + body), or a caller-side mistake.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PushError {
    /// Connect/read/write failure, timeout, or malformed HTTP framing.
    Io(String),
    /// The registry answered with a status the protocol does not allow.
    Http { status: u16, body: String },
    /// The request itself cannot be expressed (empty repo/tag, https, ...).
    Invalid(String),
}

impl fmt::Display for PushError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(m) => write!(f, "registry transport error: {m}"),
            Self::Http { status, body } => write!(f, "registry answered {status}: {body}"),
            Self::Invalid(m) => write!(f, "invalid push request: {m}"),
        }
    }
}

impl std::error::Error for PushError {}

// ---- public verbs ---------------------------------------------------------

/// `GET /v2/` must answer 200 (the V2 protocol liveness probe).
pub async fn ping(registry_addr: &str) -> Result<(), PushError> {
    let target = Target {
        authority: normalize_authority(registry_addr)?,
        path: "/v2/".to_string(),
    };
    let reply = exchange(&target, "GET", &[], b"").await?;
    expect_status(&reply, &[200])
}

/// Push one image as `repo:tag`, returning the manifest digest
/// (`sha256:<64 hex>`, the registry's `Docker-Content-Digest`).
///
/// Blobs go through the chunked POST/PATCH/PUT flow (idempotent: an
/// already-present blob simply commits again), then a docker v2
/// schema-2 manifest is published by tag. Re-pushing the same image is
/// safe and returns the same digest.
pub async fn push_image(
    registry_addr: &str,
    repo: &str,
    tag: &str,
    image: &PushImage,
) -> Result<String, PushError> {
    let authority = normalize_authority(registry_addr)?;
    validate_repo_tag(repo, tag)?;

    let config_digest = upload_blob(&authority, repo, &image.config).await?;
    let mut layer_digests = Vec::with_capacity(image.layers.len());
    for layer in &image.layers {
        layer_digests.push(upload_blob(&authority, repo, &layer.data).await?);
    }

    let manifest = build_manifest(image, &config_digest, &layer_digests)?;
    let computed = sha256_digest(&manifest);
    let target = Target {
        authority,
        path: format!("/v2/{repo}/manifests/{tag}"),
    };
    let reply = exchange(
        &target,
        "PUT",
        &[("content-type", MANIFEST_MEDIA_TYPE)],
        &manifest,
    )
    .await?;
    // distribution answers 201; some proxies rewrite a PUT into a 200.
    expect_status(&reply, &[200, 201])?;
    Ok(reply
        .header("docker-content-digest")
        .filter(|d| !d.is_empty())
        .map(str::to_string)
        .unwrap_or(computed))
}

// ---- blob upload ----------------------------------------------------------

/// Upload one blob through POST -> PATCH -> PUT?digest= and return its
/// `sha256:<hex>` digest.
async fn upload_blob(authority: &str, repo: &str, data: &[u8]) -> Result<String, PushError> {
    let digest = sha256_digest(data);
    let create = Target {
        authority: authority.to_string(),
        path: format!("/v2/{repo}/blobs/uploads/"),
    };
    let reply = exchange(&create, "POST", &[], b"").await?;
    expect_status(&reply, &[202])?;
    let patch = resolve_location(authority, location_of(&reply)?)?;

    let reply = exchange(
        &patch,
        "PATCH",
        &[("content-type", "application/octet-stream")],
        data,
    )
    .await?;
    expect_status(&reply, &[202])?;
    // The server may hand back a fresh (still relative) Location after
    // the chunk; honour it, else commit against the PATCH target.
    let commit = match reply.header("location") {
        Some(loc) => resolve_location(authority, loc)?,
        None => patch,
    };
    let sep = if commit.path.contains('?') { '&' } else { '?' };
    let commit = Target {
        authority: commit.authority,
        path: format!(
            "{}{sep}digest={}",
            commit.path,
            percent_encode_query(&digest)
        ),
    };
    let reply = exchange(&commit, "PUT", &[], b"").await?;
    expect_status(&reply, &[201])?;
    Ok(digest)
}

// ---- manifest building (pure) --------------------------------------------

/// Build the docker v2 schema-2 manifest bytes for `image` (pure, so the
/// digest is taken over exactly the bytes that get PUT).
fn build_manifest(
    image: &PushImage,
    config_digest: &str,
    layer_digests: &[String],
) -> Result<Vec<u8>, PushError> {
    let layers: Vec<serde_json::Value> = image
        .layers
        .iter()
        .zip(layer_digests)
        .map(|(layer, digest)| descriptor(&layer.media_type, layer.data.len(), digest))
        .collect();
    serde_json::to_vec(&json!({
        "schemaVersion": 2,
        "mediaType": MANIFEST_MEDIA_TYPE,
        "config": descriptor(&image.config_media_type, image.config.len(), config_digest),
        "layers": layers,
    }))
    .map_err(|e| PushError::Invalid(format!("manifest serialization failed: {e}")))
}

fn descriptor(media_type: &str, size: usize, digest: &str) -> serde_json::Value {
    json!({"mediaType": media_type, "size": size, "digest": digest})
}

/// `sha256:<64 lowercase hex>` content digest of `data`.
pub fn sha256_digest(data: &[u8]) -> String {
    let out = Sha256::digest(data);
    let mut s = String::with_capacity(71);
    s.push_str("sha256:");
    for b in out {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Percent-encode a query value; `sha256:ab` becomes `sha256%3Aab`.
fn percent_encode_query(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for b in value.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

// ---- request validation ---------------------------------------------------

/// Accept `host:port`, `http://host:port` or a trailing-slash variant;
/// reject empty, `https://` (T8.1, Q38) and anything that cannot be a Host.
fn normalize_authority(registry_addr: &str) -> Result<String, PushError> {
    let trimmed = registry_addr.trim();
    if trimmed.is_empty() {
        return Err(PushError::Invalid(
            "registry address must not be empty".to_string(),
        ));
    }
    if trimmed.starts_with("https://") {
        return Err(PushError::Invalid(
            "https is not supported (plain HTTP only; T8.1, Q38)".to_string(),
        ));
    }
    let authority = trimmed
        .strip_prefix("http://")
        .unwrap_or(trimmed)
        .trim_end_matches('/');
    let unusable = authority.is_empty()
        || authority
            .bytes()
            .any(|b| b.is_ascii_whitespace() || b.is_ascii_control() || b == b'/' || b == b'?');
    if unusable {
        return Err(PushError::Invalid(format!(
            "unusable registry address {registry_addr:?}"
        )));
    }
    Ok(authority.to_string())
}

/// Reject requests the protocol cannot express. The name/tag grammars
/// are the same ones the server enforces (see `validate.rs`).
fn validate_repo_tag(repo: &str, tag: &str) -> Result<(), PushError> {
    if repo.trim().is_empty() {
        return Err(PushError::Invalid("repo must not be empty".to_string()));
    }
    if !crate::validate::valid_repo_name(repo) {
        return Err(PushError::Invalid(format!(
            "{repo:?} is not a valid repository name"
        )));
    }
    if tag.trim().is_empty() {
        return Err(PushError::Invalid("tag must not be empty".to_string()));
    }
    if !crate::validate::valid_tag(tag) {
        return Err(PushError::Invalid(format!("{tag:?} is not a valid tag")));
    }
    Ok(())
}

fn location_of(reply: &Reply) -> Result<&str, PushError> {
    reply.header("location").ok_or_else(|| {
        PushError::Io(format!(
            "registry answered {} without a Location header",
            reply.status
        ))
    })
}

fn expect_status(reply: &Reply, expected: &[u16]) -> Result<(), PushError> {
    if expected.contains(&reply.status) {
        return Ok(());
    }
    Err(PushError::Http {
        status: reply.status,
        body: reply.body_text(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_is_sha256_hex() {
        assert_eq!(
            sha256_digest(b""),
            "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        let d = sha256_digest(b"abc");
        assert_eq!(d.len(), 7 + 64);
        assert!(d.starts_with("sha256:ba7816bf8f01cfea"));
        assert!(d[7..]
            .bytes()
            .all(|b| b.is_ascii_digit() || b.is_ascii_lowercase()));
    }

    #[test]
    fn digest_query_value_is_percent_encoded() {
        assert_eq!(percent_encode_query("sha256:ab"), "sha256%3Aab");
        assert_eq!(percent_encode_query("aA0-._~"), "aA0-._~");
        assert_eq!(percent_encode_query("a b"), "a%20b");
    }

    #[test]
    fn manifest_shape_matches_docker_schema2() {
        let image = PushImage {
            config: b"{}".to_vec(),
            config_media_type: CONFIG_MEDIA_TYPE.to_string(),
            layers: vec![LayerBlob {
                media_type: LAYER_MEDIA_TYPE.to_string(),
                data: b"layer".to_vec(),
            }],
        };
        let cfg_d = sha256_digest(&image.config);
        let lyr_d = sha256_digest(&image.layers[0].data);
        let raw = build_manifest(&image, &cfg_d, std::slice::from_ref(&lyr_d)).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&raw).unwrap();
        assert_eq!(v["schemaVersion"], 2);
        assert_eq!(v["mediaType"], MANIFEST_MEDIA_TYPE);
        assert_eq!(v["config"]["mediaType"], CONFIG_MEDIA_TYPE);
        assert_eq!(v["config"]["size"], 2);
        assert_eq!(v["config"]["digest"], cfg_d);
        assert_eq!(v["layers"][0]["mediaType"], LAYER_MEDIA_TYPE);
        assert_eq!(v["layers"][0]["size"], 5);
        assert_eq!(v["layers"][0]["digest"], lyr_d);
    }

    #[test]
    fn manifest_without_layers_is_still_valid() {
        let image = PushImage {
            config: b"{}".to_vec(),
            config_media_type: CONFIG_MEDIA_TYPE.to_string(),
            layers: vec![],
        };
        let raw = build_manifest(&image, &sha256_digest(b"{}"), &[]).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&raw).unwrap();
        assert_eq!(v["layers"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn authority_forms() {
        assert_eq!(
            normalize_authority("127.0.0.1:5000").unwrap(),
            "127.0.0.1:5000"
        );
        assert_eq!(normalize_authority("http://h:1/").unwrap(), "h:1");
        for bad in ["", "https://h:1", "a/b", "a b:1"] {
            assert!(normalize_authority(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn repo_tag_validation() {
        assert!(validate_repo_tag("demo/app", "v1.2-rc_1").is_ok());
        assert!(validate_repo_tag("", "v1").is_err());
        assert!(validate_repo_tag("demo", "").is_err());
        assert!(validate_repo_tag("Demo/App", "v1").is_err());
        assert!(validate_repo_tag("demo", "bad tag").is_err());
    }

    #[test]
    fn error_display_and_egress() {
        let e = PushError::Http {
            status: 404,
            body: "missing".into(),
        };
        assert_eq!(e.to_string(), "registry answered 404: missing");
        assert!(PushError::Io("boom".into()).to_string().contains("boom"));
        assert!(PushError::Invalid("bad".into()).to_string().contains("bad"));
        let _: &dyn std::error::Error = &e;
    }
}
