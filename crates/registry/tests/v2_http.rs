//! HTTP conformance tests for the embedded registry over a REAL socket
//! (TODO **T6.3**, Q40): the V2 ping/catalog handshake, repository-name
//! validation, and the blob plane (monolithic upload, resumable
//! POST/PATCH/PUT session, cross-repo mount, unknown-session errors).
//! Manifest + retention semantics live in `v2_manifest.rs`; the shared
//! raw client and fixtures live in `support/`.

mod support;

use support::*;

// ---- tests -----------------------------------------------------------------

#[tokio::test]
async fn ping_catalog_and_validation() {
    let (addr, _root) = boot(10).await;
    let r = req(&addr, "GET", "/v2/", &[], b"").await;
    assert_eq!(r.status, 200);
    assert_eq!(
        r.header("docker-distribution-api-version"),
        Some("registry/2.0")
    );

    let r = req(&addr, "GET", "/v2/_catalog", &[], b"").await;
    assert_eq!(r.status, 200);
    assert!(r.body_str().contains("\"repositories\":[]"));

    // invalid repo name -> 400 before any fs touch
    let r = req(&addr, "GET", "/v2/BAD/tags/list", &[], b"").await;
    assert_eq!(r.status, 400);
    assert!(r.body_str().contains("NAME_INVALID"));

    // unknown repo -> 404 NAME_UNKNOWN
    let r = req(&addr, "GET", "/v2/good-name/tags/list", &[], b"").await;
    assert_eq!(r.status, 404);
    assert!(r.body_str().contains("NAME_UNKNOWN"));
}

#[tokio::test]
async fn blob_upload_flows() {
    let (addr, _root) = boot(10).await;
    let data = b"blob-content-123".to_vec();
    let d = sha256_hex(&data);

    // monolithic POST
    let r = push_blob(&addr, "app/one", &data).await;
    assert_eq!(r.status, 201);
    assert_eq!(r.header("docker-content-digest"), Some(d.as_str()));

    // HEAD + GET
    let r = req(&addr, "HEAD", &format!("/v2/app/one/blobs/{d}"), &[], b"").await;
    assert_eq!(r.status, 200);
    assert_eq!(
        r.header("content-length"),
        Some(data.len().to_string().as_str())
    );
    let r = req(&addr, "GET", &format!("/v2/app/one/blobs/{d}"), &[], b"").await;
    assert_eq!(r.status, 200);
    assert_eq!(r.body, data);

    // resumable session: POST -> PATCH -> PUT?digest
    let data2 = b"resumable-chunk-A-chunk-B".to_vec();
    let d2 = sha256_hex(&data2);
    let r = req(&addr, "POST", "/v2/app/one/blobs/uploads/", &[], b"").await;
    assert_eq!(r.status, 202);
    let loc = r.header("location").unwrap().to_string();
    let uuid = loc.rsplit('/').next().unwrap().to_string();
    let r = req(
        &addr,
        "PATCH",
        &loc,
        &[("content-range", &format!("0-{}", data2.len() / 2 - 1))],
        &data2[..data2.len() / 2],
    )
    .await;
    assert_eq!(r.status, 202, "{}", r.body_str());
    assert_eq!(
        r.header("range"),
        Some(format!("0-{}", data2.len() / 2 - 1).as_str())
    );
    let r = req(
        &addr,
        "PUT",
        &format!("{loc}?digest={d2}"),
        &[(
            "content-range",
            &format!("{}-{}", data2.len() / 2, data2.len() - 1),
        )],
        &data2[data2.len() / 2..],
    )
    .await;
    assert_eq!(r.status, 201, "{}", r.body_str());

    // wrong-digest finalize -> DIGEST_INVALID
    let r = req(&addr, "POST", "/v2/app/one/blobs/uploads/", &[], b"").await;
    let loc = r.header("location").unwrap().to_string();
    let wrong = sha256_hex(b"other");
    let r = req(&addr, "PUT", &format!("{loc}?digest={wrong}"), &[], b"junk").await;
    assert_eq!(r.status, 400);
    assert!(r.body_str().contains("DIGEST_INVALID"));

    // cross-repo mount of an existing blob
    let r = req(
        &addr,
        "POST",
        &format!("/v2/app/two/blobs/uploads/?mount={d}&from=app/one"),
        &[],
        b"",
    )
    .await;
    assert_eq!(r.status, 201);

    // unknown session -> BLOB_UPLOAD_UNKNOWN
    let r = req(
        &addr,
        "PATCH",
        "/v2/app/one/blobs/uploads/nope-404",
        &[],
        b"x",
    )
    .await;
    assert_eq!(r.status, 404);
    assert!(r.body_str().contains("BLOB_UPLOAD_UNKNOWN"));
    let _ = uuid;
}

/// B2: GET streams the blob from disk instead of buffering it, while
/// still advertising the exact size up front (registries rely on
/// `content-length` + `docker-content-digest` for layer pulls).
#[tokio::test]
async fn blob_get_streams_large_payload() {
    let (addr, _root) = boot(10).await;
    let payload: Vec<u8> = (0..2 * 1024 * 1024).map(|i| (i % 251) as u8).collect();
    let digest = sha256_hex(&payload);

    let r = push_blob(&addr, "app/big", &payload).await;
    assert_eq!(r.status, 201, "{}", r.body_str());

    let r = req(
        &addr,
        "GET",
        &format!("/v2/app/big/blobs/{digest}"),
        &[],
        b"",
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.body_str());
    assert_eq!(r.header("content-length"), Some("2097152"));
    assert_eq!(r.header("docker-content-digest"), Some(digest.as_str()));
    assert_eq!(r.header("content-type"), Some("application/octet-stream"));
    assert_eq!(r.body.len(), payload.len());
    assert!(r.body == payload, "streamed body differs from upload");
}
