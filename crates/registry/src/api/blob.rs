//! Blob handlers (Docker Registry HTTP API V2; TODO **T6.3**, Q40):
//! fetch by digest (GET + HEAD over the global content-addressed CAS,
//! with `BLOB_UNKNOWN` for misses) and delete. Upload sessions live in
//! `uploads.rs`; this module only reads and removes finished blobs.
//! Split out of `api/mod.rs` to keep every file inside the size budget.
//!
//! GET streams the blob from disk (`ReaderStream` over the CAS file)
//! with an explicit `Content-Length` taken from the same metadata read
//! that answered HEAD, so layer pulls never buffer the whole blob in
//! memory. Range/Accept-Ranges are a documented follow-up: the response
//! is a single 200 with the full body.

use axum::body::Body;
use axum::http::StatusCode;
use axum::response::Response;
use tokio_util::io::ReaderStream;

use super::{respond, v2_error, RegState};
use crate::store;
use crate::validate;

pub(super) async fn fetch(st: &RegState, repo: &str, digest: &str, head: bool) -> Response {
    if !validate::valid_repo_name(repo) {
        return v2_error(
            StatusCode::BAD_REQUEST,
            "NAME_INVALID",
            "invalid repository name",
        );
    }
    if !validate::valid_digest(digest) {
        return v2_error(
            StatusCode::BAD_REQUEST,
            "DIGEST_INVALID",
            "malformed digest",
        );
    }
    let len = match store::blob_len(&st.root, digest).await {
        Ok(l) => l,
        Err(_) => {
            return v2_error(
                StatusCode::NOT_FOUND,
                "BLOB_UNKNOWN",
                "blob unknown to registry",
            )
        }
    };
    if head {
        respond(
            StatusCode::OK,
            &[
                ("Docker-Content-Digest", digest.to_string()),
                ("Content-Type", "application/octet-stream".into()),
                ("Content-Length", len.to_string()),
            ],
            "",
        )
    } else {
        match tokio::fs::File::open(store::blob_path(&st.root, digest)).await {
            Ok(file) => {
                let stream = ReaderStream::new(file);
                respond(
                    StatusCode::OK,
                    &[
                        ("Docker-Content-Digest", digest.to_string()),
                        ("Content-Type", "application/octet-stream".into()),
                        ("Content-Length", len.to_string()),
                    ],
                    Body::from_stream(stream),
                )
            }
            Err(_) => v2_error(
                StatusCode::NOT_FOUND,
                "BLOB_UNKNOWN",
                "blob unknown to registry",
            ),
        }
    }
}

pub(super) async fn delete(st: &RegState, repo: &str, digest: &str) -> Response {
    if !validate::valid_repo_name(repo) {
        return v2_error(
            StatusCode::BAD_REQUEST,
            "NAME_INVALID",
            "invalid repository name",
        );
    }
    if !validate::valid_digest(digest) {
        return v2_error(
            StatusCode::BAD_REQUEST,
            "DIGEST_INVALID",
            "malformed digest",
        );
    }
    let _ = store::delete_blob(&st.root, digest).await;
    respond(StatusCode::ACCEPTED, &[], "{}")
}
