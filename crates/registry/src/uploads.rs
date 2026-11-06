//! Blob upload sessions (Docker Registry HTTP API V2; TODO **T6.3**,
//! decision **Q40**).
//!
//! Three shapes, all hitting the same handlers: monolithic single-POST
//! (`POST .../blobs/uploads/?digest=`), cross-repo mount
//! (`?mount=<digest>&from=<repo>` — trivial against the global CAS),
//! and the resumable POST -> PATCH(+) -> PUT?digest= dance docker and
//! containerd actually perform. Session state is the staging file
//! itself (`uploads/<uuid>`): no in-memory maps, so a server restart
//! simply sweeps the leftovers (gc.rs).

use std::collections::HashMap;
use std::io;
use std::path::Path;

use axum::body::Body;
use axum::extract::Request;
use axum::http::StatusCode;
use axum::response::Response;
use tokio::io::AsyncWriteExt;
use tokio_stream::StreamExt as _;

use crate::api::{respond, v2_error, RegState, PUSH_SEQ};
use crate::store;
use crate::validate;

fn new_uuid() -> String {
    format!(
        "{}-{:x}",
        crate::now_millis(),
        PUSH_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    )
}

fn valid_uuid(u: &str) -> bool {
    !u.is_empty() && u.len() <= 64 && u.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// Percent-decode a query component (digests arrive as `sha256%3A...`).
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let h = (b[i + 1] as char).to_digit(16);
            let l = (b[i + 2] as char).to_digit(16);
            if let (Some(h), Some(l)) = (h, l) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub(crate) fn query_map(q: Option<&str>) -> HashMap<String, String> {
    let mut m = HashMap::new();
    let Some(q) = q else { return m };
    for pair in q.split('&') {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        m.insert(percent_decode(k), percent_decode(v));
    }
    m
}

/// Stream a request body onto the end of a staging file; returns the
/// bytes appended (blobs are never buffered whole in memory).
async fn append_body(body: Body, path: &Path) -> io::Result<u64> {
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await?;
    let mut stream = body.into_data_stream();
    let mut n = 0u64;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(io::Error::other)?;
        file.write_all(&chunk).await?;
        n += chunk.len() as u64;
    }
    file.flush().await?;
    Ok(n)
}

fn range_start(v: &str) -> Option<u64> {
    let first = v.split('/').next()?;
    let (a, _) = first.split_once('-')?;
    a.parse().ok()
}

/// POST `/v2/<name>/blobs/uploads/` — start a session, mount, or
/// complete a monolithic push in one round-trip.
pub(crate) async fn create(st: &RegState, repo: &str, query: Option<&str>, body: Body) -> Response {
    if !validate::valid_repo_name(repo) {
        return v2_error(
            StatusCode::BAD_REQUEST,
            "NAME_INVALID",
            "invalid repository name",
        );
    }
    let q = query_map(query);
    if let Err(e) = store::ensure_uploads(&st.root).await {
        tracing::warn!(target: "init-pro", error = %e, "registry: uploads dir heal failed");
    }
    if let (Some(mount), Some(from)) = (q.get("mount"), q.get("from")) {
        if validate::valid_digest(mount)
            && validate::valid_repo_name(from)
            && store::blob_exists(&st.root, mount).await
        {
            return respond(
                StatusCode::CREATED,
                &[
                    ("Location", format!("/v2/{repo}/blobs/{mount}")),
                    ("Docker-Content-Digest", mount.clone()),
                ],
                "",
            );
        }
        // Mount miss falls through to a normal session (spec-allowed).
    }
    if let Some(digest) = q.get("digest") {
        if !validate::valid_digest(digest) {
            return v2_error(
                StatusCode::BAD_REQUEST,
                "DIGEST_INVALID",
                "malformed digest",
            );
        }
        let uuid = new_uuid();
        let staged = store::upload_file(&st.root, &uuid);
        if let Err(e) = append_body(body, &staged).await {
            tracing::warn!(target: "init-pro", error = %e, "registry: monolithic upload failed");
            return v2_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "UNKNOWN",
                "upload staging failed",
            );
        }
        return match store::hash_file(&staged).await {
            Ok(actual) if actual == *digest => {
                match store::commit_blob(&st.root, &staged, digest).await {
                    Ok(()) => respond(
                        StatusCode::CREATED,
                        &[
                            ("Location", format!("/v2/{repo}/blobs/{digest}")),
                            ("Docker-Content-Digest", digest.clone()),
                        ],
                        "",
                    ),
                    Err(_) => v2_error(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "UNKNOWN",
                        "blob commit failed",
                    ),
                }
            }
            _ => {
                let _ = tokio::fs::remove_file(&staged).await;
                v2_error(
                    StatusCode::BAD_REQUEST,
                    "DIGEST_INVALID",
                    "digest does not match uploaded content",
                )
            }
        };
    }
    let uuid = new_uuid();
    let staged = store::upload_file(&st.root, &uuid);
    if let Err(e) = tokio::fs::File::create(&staged).await {
        tracing::warn!(target: "init-pro", error = %e, "registry: upload session create failed");
        return v2_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "UNKNOWN",
            "upload staging failed",
        );
    }
    respond(
        StatusCode::ACCEPTED,
        &[
            ("Location", format!("/v2/{repo}/blobs/uploads/{uuid}")),
            ("Range", "0-0".to_string()),
            ("Docker-Upload-UUID", uuid),
        ],
        "",
    )
}

async fn current_len(st: &RegState, uuid: &str) -> Option<u64> {
    if !valid_uuid(uuid) {
        return None;
    }
    tokio::fs::metadata(store::upload_file(&st.root, uuid))
        .await
        .ok()
        .map(|m| m.len())
}

/// PATCH `/v2/<name>/blobs/uploads/<uuid>` — append streamed chunks.
pub(crate) async fn patch(st: &RegState, repo: &str, uuid: &str, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let Some(cur) = current_len(st, uuid).await else {
        return v2_error(
            StatusCode::NOT_FOUND,
            "BLOB_UPLOAD_UNKNOWN",
            "upload session not found",
        );
    };
    if let Some(start) = parts
        .headers
        .get("content-range")
        .and_then(|v| v.to_str().ok())
        .and_then(range_start)
    {
        if start != cur {
            return respond(
                StatusCode::RANGE_NOT_SATISFIABLE,
                &[("Range", format!("0-{}", cur.saturating_sub(1)))],
                format!("{{\"errors\":[{{\"code\":\"RANGE_INVALID\",\"message\":\"offset {start} does not match current {cur}\"}}]}}"),
            );
        }
    }
    let staged = store::upload_file(&st.root, uuid);
    match append_body(body, &staged).await {
        Ok(n) => {
            let end = cur + n;
            let range = if end > 0 {
                vec![("Range", format!("0-{}", end - 1))]
            } else {
                vec![]
            };
            let mut headers = range;
            headers.push(("Location", format!("/v2/{repo}/blobs/uploads/{uuid}")));
            headers.push(("Docker-Upload-UUID", uuid.to_string()));
            respond(StatusCode::ACCEPTED, &headers, "")
        }
        Err(e) => {
            tracing::warn!(target: "init-pro", error = %e, "registry: upload append failed");
            v2_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "UNKNOWN",
                "upload append failed",
            )
        }
    }
}

/// PUT `/v2/<name>/blobs/uploads/<uuid>?digest=` — finalize into the CAS.
pub(crate) async fn finish(
    st: &RegState,
    repo: &str,
    uuid: &str,
    query: Option<&str>,
    req: Request,
) -> Response {
    let q = query_map(query);
    let Some(digest) = q.get("digest") else {
        return v2_error(
            StatusCode::BAD_REQUEST,
            "DIGEST_INVALID",
            "digest query parameter is required",
        );
    };
    if !validate::valid_digest(digest) {
        return v2_error(
            StatusCode::BAD_REQUEST,
            "DIGEST_INVALID",
            "malformed digest",
        );
    }
    let (parts, body) = req.into_parts();
    let Some(cur) = current_len(st, uuid).await else {
        return v2_error(
            StatusCode::NOT_FOUND,
            "BLOB_UPLOAD_UNKNOWN",
            "upload session not found",
        );
    };
    if let Some(start) = parts
        .headers
        .get("content-range")
        .and_then(|v| v.to_str().ok())
        .and_then(range_start)
    {
        if start != cur {
            return v2_error(
                StatusCode::RANGE_NOT_SATISFIABLE,
                "RANGE_INVALID",
                "offset does not match current",
            );
        }
    }
    let staged = store::upload_file(&st.root, uuid);
    if let Err(e) = append_body(body, &staged).await {
        tracing::warn!(target: "init-pro", error = %e, "registry: upload finalize append failed");
        return v2_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "UNKNOWN",
            "upload append failed",
        );
    }
    match store::hash_file(&staged).await {
        Ok(actual) if actual == *digest => {
            match store::commit_blob(&st.root, &staged, digest).await {
                Ok(()) => respond(
                    StatusCode::CREATED,
                    &[
                        ("Location", format!("/v2/{repo}/blobs/{digest}")),
                        ("Docker-Content-Digest", digest.clone()),
                    ],
                    "",
                ),
                Err(e) => {
                    tracing::warn!(target: "init-pro", error = %e, "registry: blob commit failed");
                    v2_error(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "UNKNOWN",
                        "blob commit failed",
                    )
                }
            }
        }
        _ => {
            let _ = tokio::fs::remove_file(&staged).await;
            v2_error(
                StatusCode::BAD_REQUEST,
                "DIGEST_INVALID",
                "digest does not match uploaded content",
            )
        }
    }
}

/// DELETE `/v2/<name>/blobs/uploads/<uuid>` — cancel a session.
pub(crate) async fn cancel(st: &RegState, _repo: &str, uuid: &str) -> Response {
    if current_len(st, uuid).await.is_none() {
        return v2_error(
            StatusCode::NOT_FOUND,
            "BLOB_UPLOAD_UNKNOWN",
            "upload session not found",
        );
    }
    let _ = store::delete_upload(&st.root, uuid).await;
    respond(StatusCode::NO_CONTENT, &[], "")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuids_and_ranges() {
        assert!(valid_uuid("123-abc"));
        assert!(!valid_uuid(""));
        assert!(!valid_uuid("../etc"));
        assert!(!valid_uuid(&"x".repeat(65)));
        assert_eq!(range_start("0-668"), Some(0));
        assert_eq!(range_start("669-1337/2000"), Some(669));
        assert_eq!(range_start("junk"), None);
    }

    #[test]
    fn query_decoding() {
        let q = query_map(Some("digest=sha256%3Aaa&mount=sha256%3Abb&empty="));
        assert_eq!(q.get("digest").unwrap(), "sha256:aa");
        assert_eq!(q.get("mount").unwrap(), "sha256:bb");
        assert_eq!(q.get("empty").unwrap(), "");
        assert!(query_map(None).is_empty());
    }
}
