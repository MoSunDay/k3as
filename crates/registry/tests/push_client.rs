//! Push-client conformance for the T6.3 M1 slice: a real in-process
//! registry, a synthetic `docker save` tarball, and the pure-Rust push
//! client (`registry::push_image`) end to end over plain HTTP (T8.1, Q38).
//!
//! The client itself enforces the protocol statuses (202 POST, 202
//! PATCH, 201 blob commit, 200/201 manifest), so a successful second
//! push IS the "201 both times" assertion; the checks here pin the
//! observable outcome: digest shape, tag visibility and the byte-exact
//! manifest the registry stored.

mod support;

use std::io::Write as _;

use registry::client::LAYER_MEDIA_TYPE;
use registry::{parse_docker_save, to_push_image, LayerBlob, PushError, PushImage};

const CONFIG: &[u8] = br#"{"architecture":"amd64","os":"linux"}"#;
const LAYER_ONE: &[u8] = b"first-layer-bytes";
const LAYER_TWO: &[u8] = b"second-layer-bytes";
const MANIFEST: &[u8] =
    br#"[{"Config":"cfg.json","RepoTags":["demo/app:v1"],"Layers":["l1/layer.tar","l2/layer.tar"]}]"#;

fn append(b: &mut tar::Builder<&mut Vec<u8>>, name: &str, data: &[u8]) {
    let mut h = tar::Header::new_gnu();
    h.set_size(data.len() as u64);
    h.set_mode(0o644);
    h.set_cksum();
    b.append_data(&mut h, name, std::io::Cursor::new(data))
        .unwrap();
}

/// A two-layer docker-save archive, optionally gzip-wrapped.
fn synthetic_save(gzip: bool) -> Vec<u8> {
    let mut tar = Vec::new();
    {
        let mut b = tar::Builder::new(&mut tar);
        append(&mut b, "manifest.json", MANIFEST);
        append(&mut b, "cfg.json", CONFIG);
        append(&mut b, "l1/layer.tar", LAYER_ONE);
        append(&mut b, "l2/layer.tar", LAYER_TWO);
        b.finish().unwrap();
    }
    if !gzip {
        return tar;
    }
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    enc.write_all(&tar).unwrap();
    enc.finish().unwrap()
}

fn assert_digest_shape(digest: &str) {
    assert!(digest.starts_with("sha256:"), "digest {digest}");
    assert_eq!(digest.len(), 7 + 64, "digest {digest}");
    assert!(
        digest[7..]
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "digest {digest} is not lowercase hex"
    );
}

#[tokio::test]
async fn push_docker_save_roundtrip_is_idempotent() {
    let (addr, _root) = support::boot(10).await;
    registry::ping(&addr).await.unwrap();

    // gzip-wrapped archive through the importer, then the push client.
    let save = parse_docker_save(&synthetic_save(true)).unwrap();
    assert_eq!(save.repo_tags, vec!["demo/app:v1".to_string()]);
    assert_eq!(save.layers.len(), 2);
    let image = to_push_image(&save);

    let digest = registry::push_image(&addr, "demo/app", "v1", &image)
        .await
        .unwrap();
    assert_digest_shape(&digest);

    // The tag is visible and points at exactly this digest.
    let tags = support::tags(&addr, "demo/app").await;
    assert!(tags.contains(&"v1".to_string()), "tags {tags:?}");
    assert_eq!(
        support::sha256_digest_of_tag(&addr, "demo/app", "v1").await,
        digest
    );

    // The stored manifest bytes hash to the digest the client returned.
    let r = support::req(&addr, "GET", "/v2/demo/app/manifests/v1", &[], b"").await;
    assert_eq!(r.status, 200, "{}", r.body_str());
    assert_eq!(support::sha256_hex(&r.body), digest);
    let manifest: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
    assert_eq!(manifest["schemaVersion"], 2);
    assert_eq!(
        manifest["config"]["mediaType"],
        registry::client::CONFIG_MEDIA_TYPE
    );
    assert_eq!(manifest["config"]["size"], CONFIG.len());
    assert_eq!(manifest["layers"].as_array().unwrap().len(), 2);
    assert_eq!(
        manifest["layers"][0]["mediaType"],
        registry::client::LAYER_MEDIA_TYPE
    );
    assert_eq!(manifest["layers"][1]["size"], LAYER_TWO.len());

    // Same image again: same digest, and an independent re-PUT of that
    // manifest is accepted with 201 (the registry does not reject
    // duplicate pushes).
    let again = registry::push_image(&addr, "demo/app", "v1", &image)
        .await
        .unwrap();
    assert_eq!(again, digest);
    let raw = support::req(
        &addr,
        "PUT",
        "/v2/demo/app/manifests/v1",
        &[("content-type", registry::client::MANIFEST_MEDIA_TYPE)],
        &r.body,
    )
    .await;
    assert_eq!(raw.status, 201, "{}", raw.body_str());

    // The plain (uncompressed) archive imports and pushes too, under its
    // own tag, and yields the same manifest digest (same content).
    let plain = parse_docker_save(&synthetic_save(false)).unwrap();
    // Staged paths differ per import; content identity is size + digest.
    let shape = |s: &registry::DockerSave| {
        s.layers
            .iter()
            .map(|l| (l.size, l.digest.clone()))
            .collect::<Vec<_>>()
    };
    assert_eq!(shape(&plain), shape(&save));
    let plain_digest = registry::push_image(&addr, "demo/app", "v1-plain", &to_push_image(&plain))
        .await
        .unwrap();
    assert_eq!(plain_digest, digest);
}

#[tokio::test]
async fn push_rejects_bad_input_and_dead_registries() {
    let (addr, _root) = support::boot(5).await;
    let image = to_push_image(&parse_docker_save(&synthetic_save(false)).unwrap());

    match registry::push_image(&addr, "", "v1", &image).await {
        Err(PushError::Invalid(m)) => assert!(m.contains("repo"), "{m}"),
        other => panic!("expected Invalid for an empty repo, got {other:?}"),
    }
    match registry::push_image(&addr, "demo/app", "", &image).await {
        Err(PushError::Invalid(m)) => assert!(m.contains("tag"), "{m}"),
        other => panic!("expected Invalid for an empty tag, got {other:?}"),
    }
    assert!(matches!(
        registry::push_image("https://127.0.0.1:1", "demo/app", "v1", &image).await,
        Err(PushError::Invalid(_))
    ));
    // Nothing listening on port 1: the transport surfaces an Io error
    // (not a panic, not a silent success).
    assert!(matches!(
        registry::push_image("127.0.0.1:1", "demo/app", "v1", &image).await,
        Err(PushError::Io(_))
    ));
    assert!(matches!(
        registry::ping("127.0.0.1:1").await,
        Err(PushError::Io(_))
    ));
}

#[tokio::test]
async fn push_streams_a_layer_past_the_body_limit() {
    // The registry's 4 MiB axum body limit only guards buffered
    // extractors; the chunked PATCH streams, so a bigger layer must push.
    let (addr, _root) = support::boot(5).await;
    let layer: Vec<u8> = (0..5 * 1024 * 1024).map(|i| (i % 251) as u8).collect();
    let image = PushImage {
        config: br#"{"architecture":"amd64","os":"linux"}"#.to_vec(),
        config_media_type: registry::client::CONFIG_MEDIA_TYPE.to_string(),
        layers: vec![LayerBlob::bytes(LAYER_MEDIA_TYPE, layer)],
    };
    let digest = registry::push_image(&addr, "demo/big", "v1", &image)
        .await
        .unwrap();
    assert_digest_shape(&digest);
    assert_eq!(
        support::sha256_digest_of_tag(&addr, "demo/big", "v1").await,
        digest
    );
}
