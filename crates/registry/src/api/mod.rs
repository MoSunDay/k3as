//! V2 wire surface: router, fallback path dispatch (axum 0.8 catch-alls
//! must be terminal, so `/v2/<name>/...` routes are parsed here with
//! right-first marker resolution, mirroring distribution), the
//! ping/catalog/tags handlers, and the shared plumbing (response
//! builders, state, manifest media types). The manifest and blob
//! handlers live in the sibling modules so each file stays small.
//! TODO **T6.3**, Q40.

mod blob;
mod manifest;

use std::path::PathBuf;
use std::sync::atomic::AtomicU64;

use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::{Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;

use crate::store;
use crate::uploads;
use crate::validate;
use crate::RegistryConfig;

/// Manifest media types accepted on PUT (docker v2 + OCI image/index).
pub(crate) const MANIFEST_TYPES: [&str; 4] = [
    "application/vnd.docker.distribution.manifest.v2+json",
    "application/vnd.docker.distribution.manifest.list.v2+json",
    "application/vnd.oci.image.manifest.v1+json",
    "application/vnd.oci.image.index.v1+json",
];

/// Process-wide push sequence: retention tie-breaker for same-millis pushes.
pub(crate) static PUSH_SEQ: AtomicU64 = AtomicU64::new(1);

#[derive(Clone)]
pub(crate) struct RegState {
    pub root: PathBuf,
    pub cfg: RegistryConfig,
}

pub fn v2_router(root: PathBuf, cfg: RegistryConfig) -> Router {
    Router::new()
        .route("/v2", get(ping))
        .route("/v2/", get(ping))
        .route("/v2/_catalog", get(catalog))
        .fallback(dispatch)
        .layer(DefaultBodyLimit::max(4 * 1024 * 1024))
        .with_state(RegState { root, cfg })
}

// ---- plumbing -------------------------------------------------------------

/// Build a response with explicit string headers (registry header names
/// like Docker-Content-Digest are not in the http crate's constants).
pub(crate) fn respond(
    status: StatusCode,
    headers: &[(&str, String)],
    body: impl Into<Body>,
) -> Response {
    let mut b = Response::builder().status(status);
    for (k, v) in headers {
        b = b.header(*k, v.as_str());
    }
    b.body(body.into())
        .unwrap_or_else(|_| (status, "").into_response())
}

/// V2 error envelope: `{"errors":[{"code","message","detail"}]}`.
pub(crate) fn v2_error(status: StatusCode, code: &str, message: &str) -> Response {
    let body = serde_json::json!({"errors":[{"code":code,"message":message,"detail":null}]});
    respond(
        status,
        &[("Content-Type", "application/json".into())],
        body.to_string(),
    )
}

enum Target {
    TagsList { repo: String },
    Manifest { repo: String, reference: String },
    Blob { repo: String, digest: String },
    UploadCreate { repo: String },
    UploadSession { repo: String, uuid: String },
}

/// Parse a `/v2/...` path. Markers (`manifests` / `blobs` / `tags`) are
/// resolved RIGHT-first: a repo component may legitimately be named
/// `blobs` or `manifests`, and the rightmost marker wins.
fn parse_target(path: &str) -> Option<Target> {
    let rest = path.strip_prefix("/v2/")?;
    if rest.is_empty() {
        return None;
    }
    let mut segs: Vec<&str> = rest.split('/').collect();
    if segs.last() == Some(&"") {
        segs.pop(); // tolerate a trailing slash
    }
    for i in (0..segs.len()).rev() {
        match segs.get(i) {
            Some(&"manifests") if segs.len() == i + 2 => {
                return Some(Target::Manifest {
                    repo: segs[..i].join("/"),
                    reference: segs[i + 1].to_string(),
                });
            }
            Some(&"blobs") => {
                let repo = segs[..i].join("/");
                return match (segs.get(i + 1), segs.get(i + 2)) {
                    (Some(&"uploads"), None) => Some(Target::UploadCreate { repo }),
                    (Some(&"uploads"), Some(_)) if segs.len() == i + 3 => {
                        Some(Target::UploadSession {
                            repo,
                            uuid: segs[i + 2].to_string(),
                        })
                    }
                    (Some(_), None) => Some(Target::Blob {
                        repo,
                        digest: segs[i + 1].to_string(),
                    }),
                    _ => None,
                };
            }
            Some(&"tags") if segs.get(i + 1) == Some(&"list") && segs.len() == i + 2 => {
                return Some(Target::TagsList {
                    repo: segs[..i].join("/"),
                });
            }
            _ => {}
        }
    }
    None
}

async fn dispatch(st: State<RegState>, req: Request) -> Response {
    let method = req.method().clone();
    let uri = req.uri().clone();
    let m_not_allowed = |code: &str| {
        v2_error(
            StatusCode::METHOD_NOT_ALLOWED,
            code,
            "method not allowed for this route",
        )
    };
    match parse_target(uri.path()) {
        Some(Target::TagsList { repo }) if method == Method::GET => tags_list(&st, &repo).await,
        Some(Target::Manifest { repo, reference }) => match method {
            Method::GET => manifest::fetch(&st, &repo, &reference, false).await,
            Method::HEAD => manifest::fetch(&st, &repo, &reference, true).await,
            Method::PUT => manifest::put(&st, &repo, &reference, req).await,
            Method::DELETE => manifest::delete(&st, &repo, &reference).await,
            _ => m_not_allowed("UNSUPPORTED"),
        },
        Some(Target::Blob { repo, digest }) => match method {
            Method::GET => blob::fetch(&st, &repo, &digest, false).await,
            Method::HEAD => blob::fetch(&st, &repo, &digest, true).await,
            Method::DELETE => blob::delete(&st, &repo, &digest).await,
            _ => m_not_allowed("UNSUPPORTED"),
        },
        Some(Target::UploadCreate { repo }) if method == Method::POST => {
            uploads::create(&st, &repo, uri.query(), req.into_body()).await
        }
        Some(Target::UploadSession { repo, uuid }) => match method {
            Method::PATCH => uploads::patch(&st, &repo, &uuid, req).await,
            Method::PUT => uploads::finish(&st, &repo, &uuid, uri.query(), req).await,
            Method::DELETE => uploads::cancel(&st, &repo, &uuid).await,
            _ => m_not_allowed("UNSUPPORTED"),
        },
        _ => v2_error(StatusCode::NOT_FOUND, "NOT_FOUND", "unknown route"),
    }
}

// ---- basics ---------------------------------------------------------------

async fn ping() -> Response {
    respond(
        StatusCode::OK,
        &[("Docker-Distribution-API-Version", "registry/2.0".into())],
        "{}",
    )
}

async fn catalog(State(st): State<RegState>) -> Response {
    match store::list_repos(&st.root).await {
        Ok(repos) => respond(
            StatusCode::OK,
            &[("Content-Type", "application/json".into())],
            serde_json::json!({"repositories": repos}).to_string(),
        ),
        Err(_) => v2_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "UNKNOWN",
            "catalog read failed",
        ),
    }
}

async fn tags_list(st: &RegState, repo: &str) -> Response {
    if !validate::valid_repo_name(repo) {
        return v2_error(
            StatusCode::BAD_REQUEST,
            "NAME_INVALID",
            "invalid repository name",
        );
    }
    if !store::repo_exists(&st.root, repo).await {
        return v2_error(
            StatusCode::NOT_FOUND,
            "NAME_UNKNOWN",
            "repository not known to registry",
        );
    }
    match store::list_tags(&st.root, repo).await {
        Ok(tags) => respond(
            StatusCode::OK,
            &[("Content-Type", "application/json".into())],
            serde_json::json!({"name": repo, "tags": tags.into_iter().map(|(t, _)| t).collect::<Vec<_>>()}).to_string(),
        ),
        Err(_) => v2_error(StatusCode::INTERNAL_SERVER_ERROR, "UNKNOWN", "tag list failed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets_parse_right_first() {
        assert!(matches!(
            parse_target("/v2/a/b/manifests/x:1"),
            Some(Target::Manifest { .. })
        ));
        assert!(
            matches!(parse_target("/v2/a/manifests/blobs/manifests/d"), Some(Target::Manifest { ref repo, .. }) if repo == "a/manifests/blobs")
        );
        assert!(matches!(
            parse_target("/v2/a/b/tags/list"),
            Some(Target::TagsList { .. })
        ));
        assert!(matches!(
            parse_target("/v2/a/b/blobs/uploads/"),
            Some(Target::UploadCreate { .. })
        ));
        assert!(
            matches!(parse_target("/v2/a/b/blobs/uploads/u1"), Some(Target::UploadSession { uuid, .. }) if uuid == "u1")
        );
        assert!(
            matches!(parse_target("/v2/a/b/blobs/sha256:ff"), Some(Target::Blob { digest, .. }) if digest == "sha256:ff")
        );
        assert!(parse_target("/v2/").is_none());
        assert!(parse_target("/v2/only").is_none());
        assert!(parse_target("/v2/a/b/manifests").is_none());
    }
}
