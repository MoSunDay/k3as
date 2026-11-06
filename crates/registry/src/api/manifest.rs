//! Manifest handlers (Docker Registry HTTP API V2; TODO **T6.3**, Q40):
//! fetch by tag or digest (GET + HEAD, media type resolved from the
//! tag metadata or the manifest's own `mediaType`), push (PUT with
//! referential validation, digest re-check, tag write, then the
//! retention pass), and delete (digest-only, tag scrub + blob GC; an
//! unknown digest answers 404 `MANIFEST_UNKNOWN`, C4).
//! Split out of `api/mod.rs` to keep every file inside the size budget;
//! behaviour is unchanged.

use std::sync::atomic::Ordering;

use axum::body::Bytes;
use axum::extract::Request;
use axum::http::{header, StatusCode};
use axum::response::Response;

use super::{respond, v2_error, RegState, MANIFEST_TYPES, PUSH_SEQ};
use crate::gc;
use crate::store::{self, TagMeta};
use crate::validate;

pub(super) async fn fetch(st: &RegState, repo: &str, reference: &str, head: bool) -> Response {
    if !validate::valid_repo_name(repo) {
        return v2_error(
            StatusCode::BAD_REQUEST,
            "NAME_INVALID",
            "invalid repository name",
        );
    }
    let (digest, ct) = if validate::valid_digest(reference) {
        (reference.to_string(), None)
    } else if validate::valid_tag(reference) {
        match store::read_tag(&st.root, repo, reference).await {
            Ok(m) => (m.digest, m.media_type),
            Err(_) => {
                return v2_error(
                    StatusCode::NOT_FOUND,
                    "MANIFEST_UNKNOWN",
                    "manifest unknown to registry",
                )
            }
        }
    } else {
        return v2_error(
            StatusCode::BAD_REQUEST,
            "MANIFEST_INVALID",
            "reference is neither digest nor tag",
        );
    };
    let bytes = match store::read_manifest(&st.root, repo, &digest).await {
        Ok(b) => b,
        Err(_) => {
            return v2_error(
                StatusCode::NOT_FOUND,
                "MANIFEST_UNKNOWN",
                "manifest unknown to registry",
            )
        }
    };
    // Content-Type: stored per-tag when known; by-digest fetches sniff
    // the manifest's own `mediaType` field (docker v2+ / OCI carry it).
    let ct = ct
        .or_else(|| {
            serde_json::from_slice::<serde_json::Value>(&bytes)
                .ok()
                .and_then(|v| v["mediaType"].as_str().map(String::from))
                .filter(|t| MANIFEST_TYPES.contains(&t.as_str()))
        })
        .unwrap_or_else(|| MANIFEST_TYPES[0].to_string());
    if head {
        respond(
            StatusCode::OK,
            &[
                ("Docker-Content-Digest", digest),
                ("Content-Type", ct),
                ("Content-Length", bytes.len().to_string()),
            ],
            "",
        )
    } else {
        respond(
            StatusCode::OK,
            &[("Docker-Content-Digest", digest), ("Content-Type", ct)],
            Bytes::from(bytes),
        )
    }
}

pub(super) async fn put(st: &RegState, repo: &str, reference: &str, req: Request) -> Response {
    if !validate::valid_repo_name(repo) {
        return v2_error(
            StatusCode::BAD_REQUEST,
            "NAME_INVALID",
            "invalid repository name",
        );
    }
    let (parts, body) = req.into_parts();
    let bytes = match axum::body::to_bytes(body, 4 * 1024 * 1024).await {
        Ok(b) => b,
        Err(_) => {
            return v2_error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "MANIFEST_INVALID",
                "manifest exceeds 4 MiB",
            )
        }
    };
    let ct_raw = parts
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.split(';').next().unwrap_or("").trim().to_string())
        .unwrap_or_default();
    if !ct_raw.is_empty() && !MANIFEST_TYPES.contains(&ct_raw.as_str()) {
        return v2_error(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "UNSUPPORTED",
            "manifest media type not supported",
        );
    }
    let ct = if ct_raw.is_empty() {
        MANIFEST_TYPES[0].to_string()
    } else {
        ct_raw
    };
    let digest = store::sha256_hex(&bytes);
    let by_tag = if validate::valid_digest(reference) {
        if reference != digest {
            return v2_error(
                StatusCode::BAD_REQUEST,
                "DIGEST_INVALID",
                "provided digest does not match content",
            );
        }
        false
    } else if validate::valid_tag(reference) {
        true
    } else {
        return v2_error(
            StatusCode::BAD_REQUEST,
            "MANIFEST_INVALID",
            "reference is neither digest nor tag",
        );
    };
    let v: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(_) => {
            return v2_error(
                StatusCode::BAD_REQUEST,
                "MANIFEST_INVALID",
                "manifest is not valid JSON",
            )
        }
    };
    // B1: digest grammar gates every referential probe. Client-supplied
    // digests are path segments downstream (`store::blob_path` /
    // `manifest_path`), so a malformed one (`../../etc/passwd`, `sha256:zz`)
    // must be rejected here, before any filesystem touch.
    if let Some(cfg) = v["config"]["digest"].as_str() {
        if !validate::valid_digest(cfg) {
            return v2_error(
                StatusCode::BAD_REQUEST,
                "MANIFEST_INVALID",
                "config digest malformed",
            );
        }
    }
    if let Some(layers) = v["layers"].as_array() {
        for l in layers {
            if let Some(d) = l["digest"].as_str() {
                if !validate::valid_digest(d) {
                    return v2_error(
                        StatusCode::BAD_REQUEST,
                        "MANIFEST_INVALID",
                        "layer digest malformed",
                    );
                }
            }
        }
    }
    if let Some(children) = v["manifests"].as_array() {
        for c in children {
            if let Some(d) = c["digest"].as_str() {
                if !validate::valid_digest(d) {
                    return v2_error(
                        StatusCode::BAD_REQUEST,
                        "MANIFEST_INVALID",
                        "index child digest malformed",
                    );
                }
            }
        }
    }
    // Valid-but-missing referents stay 404 MANIFEST_BLOB_UNKNOWN.
    if let Some(cfg) = v["config"]["digest"].as_str() {
        if !store::blob_exists(&st.root, cfg).await {
            return v2_error(
                StatusCode::NOT_FOUND,
                "MANIFEST_BLOB_UNKNOWN",
                "config blob unknown to registry",
            );
        }
    }
    if let Some(layers) = v["layers"].as_array() {
        for l in layers {
            if let Some(d) = l["digest"].as_str() {
                if !store::blob_exists(&st.root, d).await {
                    return v2_error(
                        StatusCode::NOT_FOUND,
                        "MANIFEST_BLOB_UNKNOWN",
                        "layer blob unknown to registry",
                    );
                }
            }
        }
    }
    if let Some(children) = v["manifests"].as_array() {
        for c in children {
            if let Some(d) = c["digest"].as_str() {
                if !store::manifest_exists(&st.root, repo, d).await {
                    return v2_error(
                        StatusCode::NOT_FOUND,
                        "MANIFEST_BLOB_UNKNOWN",
                        "index child manifest unknown",
                    );
                }
            }
        }
    }
    if let Err(e) = store::put_manifest(&st.root, repo, &digest, &bytes).await {
        tracing::warn!(target: "init-pro", error = %e, "registry: manifest write failed");
        return v2_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "UNKNOWN",
            "storage failure",
        );
    }
    if by_tag {
        let meta = TagMeta {
            digest: digest.clone(),
            media_type: Some(ct),
            pushed_at: crate::now_millis(),
            seq: PUSH_SEQ.fetch_add(1, Ordering::Relaxed),
            size: bytes.len() as u64,
        };
        if let Err(e) = store::write_tag(&st.root, repo, reference, &meta).await {
            tracing::warn!(target: "init-pro", error = %e, "registry: tag write failed");
            return v2_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "UNKNOWN",
                "storage failure",
            );
        }
        if let Err(e) = gc::apply_retention(&st.root, repo, &st.cfg.keep_extra, st.cfg.retain).await
        {
            tracing::warn!(target: "init-pro", error = %e, "registry: retention pass failed (deferred)");
        }
    }
    respond(
        StatusCode::CREATED,
        &[
            ("Docker-Content-Digest", digest.clone()),
            ("Location", format!("/v2/{repo}/manifests/{digest}")),
        ],
        "",
    )
}

pub(super) async fn delete(st: &RegState, repo: &str, reference: &str) -> Response {
    if !validate::valid_repo_name(repo) {
        return v2_error(
            StatusCode::BAD_REQUEST,
            "NAME_INVALID",
            "invalid repository name",
        );
    }
    if !validate::valid_digest(reference) {
        return v2_error(
            StatusCode::BAD_REQUEST,
            "DIGEST_INVALID",
            "deleting by tag is not supported; delete by digest",
        );
    }
    // Delete is digest-only (the 400 above rejects tags). Track existence
    // across BOTH the manifest files and the tags that point at the digest:
    // a dangling tag (manifest already gone) still counts as "found" so the
    // tag scrub can complete. When neither exists the reference is unknown
    // to this repository -> 404 MANIFEST_UNKNOWN (docker/containerd expect
    // this instead of a silent 202), with no manifest deletion and no GC.
    let mut found = store::manifest_exists(&st.root, repo, reference).await;
    let tags = store::list_tags(&st.root, repo).await.unwrap_or_default();
    for (t, m) in tags {
        if m.digest == reference {
            found = true;
            let _ = store::delete_tag(&st.root, repo, &t).await;
        }
    }
    if !found {
        return v2_error(
            StatusCode::NOT_FOUND,
            "MANIFEST_UNKNOWN",
            "manifest unknown to registry",
        );
    }
    let _ = store::delete_manifest(&st.root, repo, reference).await;
    let _ = gc::gc_blobs(&st.root).await;
    respond(StatusCode::ACCEPTED, &[], "{}")
}
