//! Manifest-plane HTTP conformance tests for the embedded registry over
//! a REAL socket (TODO **T6.3**, Q40): referential validation on push,
//! push/pull round-trip with digest + media-type headers, tag retention
//! (keep newest 10, keep-forever patterns excluded from the count), and
//! delete semantics. Blob/ping coverage lives in `v2_http.rs`; the
//! shared raw client and fixtures live in `support/`.

mod support;

use serde_json::json;
use support::*;

// ---- B1 fixtures: digest-only children + indexes --------------------------

const IMAGE: &str = "application/vnd.docker.distribution.manifest.v2+json";
const LIST: &str = "application/vnd.docker.distribution.manifest.list.v2+json";

/// Push a digest-only image child (config + one layer + manifest, NO tag);
/// returns (manifest digest, layer digest).
async fn push_child(addr: &str, repo: &str, arch: &str) -> (String, String) {
    let cfg = format!(r#"{{"architecture":"{arch}","os":"linux"}}"#);
    let cfg_digest = sha256_hex(cfg.as_bytes());
    let layer = format!("layer-of-{arch}");
    let layer_digest = sha256_hex(layer.as_bytes());
    assert_eq!(push_blob(addr, repo, cfg.as_bytes()).await.status, 201);
    assert_eq!(push_blob(addr, repo, layer.as_bytes()).await.status, 201);
    let manifest = format!(
        r#"{{"schemaVersion":2,"mediaType":"{IMAGE}","config":{{"mediaType":"application/vnd.docker.container.image.v1+json","size":{},"digest":"{cfg_digest}"}},"layers":[{{"mediaType":"application/vnd.docker.image.rootfs.diff.tar.gzip","size":{},"digest":"{layer_digest}"}}]}}"#,
        cfg.len(),
        layer.len()
    );
    let digest = sha256_hex(manifest.as_bytes());
    let r = req(
        addr,
        "PUT",
        &format!("/v2/{repo}/manifests/{digest}"),
        &[("content-type", IMAGE)],
        manifest.as_bytes(),
    )
    .await;
    assert_eq!(r.status, 201, "{}", r.body_str());
    (digest, layer_digest)
}

/// Push an index listing `children` under `tag`; returns its digest.
async fn push_index(addr: &str, repo: &str, tag: &str, children: &[&str]) -> String {
    let body = json!({
        "schemaVersion": 2,
        "mediaType": LIST,
        "manifests": children
            .iter()
            .map(|d| json!({"mediaType": IMAGE, "size": 1, "digest": d}))
            .collect::<Vec<_>>(),
    });
    let r = req(
        addr,
        "PUT",
        &format!("/v2/{repo}/manifests/{tag}"),
        &[("content-type", LIST)],
        body.to_string().as_bytes(),
    )
    .await;
    assert_eq!(r.status, 201, "{}", r.body_str());
    r.header("docker-content-digest").unwrap().to_string()
}

fn blob_file(root: &std::path::Path, digest: &str) -> std::path::PathBuf {
    root.join("blobs").join(digest.replace(':', "-"))
}

/// Rewind every CAS blob by `secs` (the GC grace window protects fresh ones).
fn age_blobs(root: &std::path::Path, secs: u64) {
    for entry in std::fs::read_dir(root.join("blobs")).unwrap() {
        let f = std::fs::OpenOptions::new()
            .write(true)
            .open(entry.unwrap().path())
            .unwrap();
        f.set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(secs))
            .unwrap();
    }
}

// ---- tests -----------------------------------------------------------------

/// B1 (batch D): children that only a deleted index listed are pruned with
/// it (manifest + blobs once past the grace window); a child a surviving
/// index still lists stays, and goes when that one goes too.
#[tokio::test]
async fn deleting_an_index_prunes_its_orphaned_children() {
    let (addr, root) = boot(10).await;
    let (amd64, amd64_layer) = push_child(&addr, "multi/arch", "amd64").await;
    let (arm64, arm64_layer) = push_child(&addr, "multi/arch", "arm64").await;
    let release = push_index(&addr, "multi/arch", "release", &[&amd64, &arm64]).await;
    let shared = push_index(&addr, "multi/arch", "shared", &[&amd64]).await;
    age_blobs(&root, registry::gc::BLOB_GC_GRACE_SECS * 2);

    let r = req(
        &addr,
        "DELETE",
        &format!("/v2/multi/arch/manifests/{release}"),
        &[],
        b"",
    )
    .await;
    assert_eq!(r.status, 202, "{}", r.body_str());
    let r = req(
        &addr,
        "GET",
        &format!("/v2/multi/arch/manifests/{arm64}"),
        &[],
        b"",
    )
    .await;
    assert_eq!(r.status, 404, "the orphaned child survived its index");
    assert!(
        !blob_file(&root, &arm64_layer).exists(),
        "the orphaned child's layer blob survived"
    );
    let r = req(
        &addr,
        "GET",
        &format!("/v2/multi/arch/manifests/{amd64}"),
        &[],
        b"",
    )
    .await;
    assert_eq!(r.status, 200, "a child still listed by `shared` was pruned");
    assert!(blob_file(&root, &amd64_layer).exists());

    let r = req(
        &addr,
        "DELETE",
        &format!("/v2/multi/arch/manifests/{shared}"),
        &[],
        b"",
    )
    .await;
    assert_eq!(r.status, 202, "{}", r.body_str());
    let r = req(
        &addr,
        "GET",
        &format!("/v2/multi/arch/manifests/{amd64}"),
        &[],
        b"",
    )
    .await;
    assert_eq!(r.status, 404, "the last child survived the last index");
    assert!(!blob_file(&root, &amd64_layer).exists());

    std::fs::remove_dir_all(root.parent().unwrap()).ok();
}

#[tokio::test]
async fn manifest_roundtrip_and_retention() {
    let (addr, root) = boot(10).await;

    // referential validation: manifest referencing a missing blob -> 404
    let ghost = sha256_hex(b"never-pushed");
    let bad = format!(
        r#"{{"schemaVersion":2,"mediaType":"application/vnd.docker.distribution.manifest.v2+json","config":{{"mediaType":"application/vnd.docker.container.image.v1+json","size":2,"digest":"{ghost}"}},"layers":[]}}"#
    );
    let r = req(
        &addr,
        "PUT",
        "/v2/myapp/manifests/bad",
        &[(
            "content-type",
            "application/vnd.docker.distribution.manifest.v2+json",
        )],
        bad.as_bytes(),
    )
    .await;
    assert_eq!(r.status, 404);
    assert!(r.body_str().contains("MANIFEST_BLOB_UNKNOWN"));

    // happy path: 12 pushes, retention keeps the newest 10
    for i in 1..=12 {
        push_image(&addr, "myapp", &format!("v{i}"), &format!("u{i}")).await;
    }
    let mut got = tags(&addr, "myapp").await;
    got.sort();
    assert_eq!(
        got,
        vec!["v10", "v11", "v12", "v3", "v4", "v5", "v6", "v7", "v8", "v9"]
    );

    // fetch by tag + by digest, digest header echoes
    let d = sha256_digest_of_tag(&addr, "myapp", "v12").await;
    let r = req(&addr, "GET", "/v2/myapp/manifests/v12", &[], b"").await;
    assert_eq!(r.status, 200);
    assert_eq!(r.header("docker-content-digest"), Some(d.as_str()));
    assert_eq!(
        r.header("content-type"),
        Some("application/vnd.docker.distribution.manifest.v2+json")
    );
    assert_eq!(sha256_hex(&r.body), d);
    let r = req(&addr, "HEAD", &format!("/v2/myapp/manifests/{d}"), &[], b"").await;
    assert_eq!(r.status, 200);
    let r = req(&addr, "GET", &format!("/v2/myapp/manifests/{d}"), &[], b"").await;
    assert_eq!(r.status, 200);
    assert!(!r.body.is_empty());

    // keep.json: pinned tags survive beyond the 10 and don't count
    std::fs::write(root.join("keep.json"), r#"["myapp:golden*"]"#).unwrap();
    push_image(&addr, "myapp", "golden-1.0", "gold").await;
    push_image(&addr, "myapp", "v13", "u13").await;
    let mut got = tags(&addr, "myapp").await;
    got.sort();
    // 10 newest countable (v4..v13) + golden-1.0 pinned
    assert_eq!(got.len(), 11);
    assert!(got.contains(&"golden-1.0".to_string()));
    assert!(!got.contains(&"v3".to_string()));
    assert!(got.contains(&"v13".to_string()));

    // delete by tag -> 400; delete by digest -> 202 and tag disappears
    let r = req(&addr, "DELETE", "/v2/myapp/manifests/v13", &[], b"").await;
    assert_eq!(r.status, 400);
    let r = req(
        &addr,
        "DELETE",
        &format!("/v2/myapp/manifests/{d}"),
        &[],
        b"",
    )
    .await;
    assert_eq!(r.status, 202);
    assert!(!tags(&addr, "myapp").await.contains(&"v12".to_string()));

    // catalog lists the repo
    let r = req(&addr, "GET", "/v2/_catalog", &[], b"").await;
    assert!(r.body_str().contains("myapp"));

    std::fs::remove_dir_all(root.parent().unwrap()).ok();
}

/// C4: deleting a grammatically valid but unknown digest must answer 404
/// `MANIFEST_UNKNOWN` instead of the old blanket 202. A silent accept made
/// clients believe an absent manifest had been removed and let a stray GC
/// pass run over a repository that had nothing to collect.
#[tokio::test]
async fn delete_unknown_digest_is_404() {
    let (addr, root) = boot(10).await;
    push_image(&addr, "myapp", "keep-1.0", "c4-unknown").await;

    // valid sha256 shape, never pushed here -> 404 MANIFEST_UNKNOWN.
    let ghost = sha256_hex(b"never-pushed-delete-c4");
    let r = req(
        &addr,
        "DELETE",
        &format!("/v2/myapp/manifests/{ghost}"),
        &[],
        b"",
    )
    .await;
    assert_eq!(r.status, 404, "body: {}", r.body_str());
    assert!(r.body_str().contains("MANIFEST_UNKNOWN"));

    // the 404 path must be side-effect free: the pushed image is untouched.
    let r = req(&addr, "GET", "/v2/myapp/manifests/keep-1.0", &[], b"").await;
    assert_eq!(r.status, 200);
    assert_eq!(tags(&addr, "myapp").await, vec!["keep-1.0".to_string()]);

    // a known digest still deletes: 202 + tag scrub (unchanged semantics).
    let d = sha256_digest_of_tag(&addr, "myapp", "keep-1.0").await;
    let r = req(
        &addr,
        "DELETE",
        &format!("/v2/myapp/manifests/{d}"),
        &[],
        b"",
    )
    .await;
    assert_eq!(r.status, 202, "body: {}", r.body_str());
    let r = req(&addr, "GET", "/v2/myapp/manifests/keep-1.0", &[], b"").await;
    assert_eq!(r.status, 404);

    std::fs::remove_dir_all(root.parent().unwrap()).ok();
}

/// B1 (path traversal / malformed input): digests carried inside a pushed
/// manifest are path segments downstream, so the grammar gate must fire
/// with 400 `MANIFEST_INVALID` BEFORE any existence probe. A well-formed
/// but unknown digest keeps the 404 `MANIFEST_BLOB_UNKNOWN` semantics.
#[tokio::test]
async fn push_rejects_malformed_digests() {
    let (addr, root) = boot(10).await;
    const CT: &str = "application/vnd.docker.distribution.manifest.v2+json";

    // (a) config.digest traversal attempt -> 400, nothing escapes the root.
    let evil_cfg = r#"{"schemaVersion":2,"mediaType":"application/vnd.docker.distribution.manifest.v2+json","config":{"mediaType":"application/vnd.docker.container.image.v1+json","size":2,"digest":"../../etc/passwd"},"layers":[]}"#;
    let r = req(
        &addr,
        "PUT",
        "/v2/myapp/manifests/evil-config",
        &[("content-type", CT)],
        evil_cfg.as_bytes(),
    )
    .await;
    assert_eq!(r.status, 400, "body: {}", r.body_str());
    assert!(r.body_str().contains("MANIFEST_INVALID"));
    assert!(r.body_str().contains("config digest malformed"));
    assert!(!root.parent().unwrap().join("etc/passwd").exists());
    assert!(!root.join("repos/etc/passwd").exists());

    // (a2) layers[].digest traversal attempt -> same rejection (the config
    // blob is real so validation genuinely reaches the layer list).
    let cfg = br#"{"architecture":"amd64","os":"linux"}"#;
    assert_eq!(push_blob(&addr, "myapp", cfg).await.status, 201);
    let cfg_d = sha256_hex(cfg);
    let evil_layer = format!(
        r#"{{"schemaVersion":2,"mediaType":"application/vnd.docker.distribution.manifest.v2+json","config":{{"mediaType":"application/vnd.docker.container.image.v1+json","size":{},"digest":"{cfg_d}"}},"layers":[{{"mediaType":"application/vnd.docker.image.rootfs.diff.tar.gzip","size":2,"digest":"../../etc/passwd"}}]}}"#,
        cfg.len()
    );
    let r = req(
        &addr,
        "PUT",
        "/v2/myapp/manifests/evil-layer",
        &[("content-type", CT)],
        evil_layer.as_bytes(),
    )
    .await;
    assert_eq!(r.status, 400, "body: {}", r.body_str());
    assert!(r.body_str().contains("MANIFEST_INVALID"));
    assert!(r.body_str().contains("layer digest malformed"));

    // (a3) manifests[].digest garbage in an index -> same rejection.
    let evil_index = r#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.index.v1+json","manifests":[{"mediaType":"application/vnd.docker.distribution.manifest.v2+json","size":2,"digest":"not-a-digest"}]}"#;
    let r = req(
        &addr,
        "PUT",
        "/v2/myapp/manifests/evil-index",
        &[("content-type", "application/vnd.oci.image.index.v1+json")],
        evil_index.as_bytes(),
    )
    .await;
    assert_eq!(r.status, 400, "body: {}", r.body_str());
    assert!(r.body_str().contains("MANIFEST_INVALID"));
    assert!(r.body_str().contains("index child digest malformed"));

    // (b) valid sha256 shape but unknown -> still 404 MANIFEST_BLOB_UNKNOWN.
    let ghost = sha256_hex(b"never-pushed-malformed-digest-case");
    let bad = format!(
        r#"{{"schemaVersion":2,"mediaType":"application/vnd.docker.distribution.manifest.v2+json","config":{{"mediaType":"application/vnd.docker.container.image.v1+json","size":2,"digest":"{ghost}"}},"layers":[]}}"#
    );
    let r = req(
        &addr,
        "PUT",
        "/v2/myapp/manifests/ghost",
        &[("content-type", CT)],
        bad.as_bytes(),
    )
    .await;
    assert_eq!(r.status, 404, "body: {}", r.body_str());
    assert!(r.body_str().contains("MANIFEST_BLOB_UNKNOWN"));

    std::fs::remove_dir_all(root.parent().unwrap()).ok();
}
