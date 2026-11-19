//! T4.13 `remove_image` tests (Q53): mark-sweep semantics + the
//! Sprint-43 review follow-up in-use guard (crictl rmi semantics).
//! Split out of `backend.rs` at the 800-line rule — same shape as
//! `image/unpack.rs` + `image/unpack/tests.rs`.

use super::tests::{sandbox_entry, temp_data_dir};
use super::*;

#[tokio::test]
async fn remove_image_sweeps_uniques_keeps_shared_layers() {
    let dir = temp_data_dir("rm-image");
    std::fs::create_dir_all(&dir).unwrap();
    let backend = EngineBackend::new(
        &dir,
        "registry.local/pause:0",
        "192.0.2.7",
        "127.0.0.1:15000",
    )
    .unwrap();
    let mk_img = |reference: &str, uniq: &str| PulledImage {
        reference: reference.to_string(),
        digest: format!("sha256:{}", uniq.repeat(64)),
        config: super::super::image::Descriptor {
            media_type: "cfg".into(),
            digest: format!("sha256:{}", format!("{uniq}c").repeat(32)),
            size: 1,
            platform: None,
        },
        layers: vec![super::super::image::Descriptor {
            media_type: "layer".into(),
            digest: format!("sha256:{}", "shared".repeat(16)),
            size: 1,
            platform: None,
        }],
    };
    let gone = mk_img("127.0.0.1:15000/test/gone:v1", "a");
    let keep = mk_img("127.0.0.1:15000/test/keep:v1", "b");
    let put_blob = |digest: &str| {
        let tmp = backend.store.tmp_file();
        std::fs::write(&tmp, digest.as_bytes()).unwrap();
        backend.store.commit(&tmp, digest).unwrap();
    };
    for img in [&gone, &keep] {
        backend.store.put_ref(img).unwrap();
        put_blob(&img.digest);
        put_blob(&img.config.digest);
        std::fs::create_dir_all(backend.store.layer_dir(&img.layers[0].digest)).unwrap();
    }
    put_blob(&gone.layers[0].digest);
    put_blob(&keep.layers[0].digest);

    crate::cri_backend::CriBackend::remove_image(&backend, "127.0.0.1:15000/test/gone:v1")
        .await
        .expect("remove_image");
    // Unknown image: idempotent Ok, nothing else swept.
    crate::cri_backend::CriBackend::remove_image(&backend, "127.0.0.1:15000/test/never:v1")
        .await
        .expect("remove_image unknown is Ok");
    assert!(
        !backend.store.has_blob(&gone.digest),
        "unique manifest gone"
    );
    assert!(
        !backend.store.has_blob(&gone.config.digest),
        "unique cfg gone"
    );
    assert!(
        backend.store.has_blob(&keep.layers[0].digest),
        "shared layer blob survives"
    );
    assert!(
        backend.store.layer_dir(&keep.layers[0].digest).is_dir(),
        "shared layer dir survives"
    );
    assert_eq!(backend.store.gc().unwrap(), (0, 0, 0), "nothing left");
    let _ = std::fs::remove_dir_all(&dir);
}

/// T4.13 review follow-up: `remove_image` REFUSES while a registry
/// entry still runs the image (crictl rmi semantics) — sweeping a
/// shared layer under a live overlayfs lowerdir would corrupt the
/// mount. Removing the entry releases the image for deletion.
#[tokio::test]
async fn remove_image_refuses_in_use_image() {
    let dir = temp_data_dir("rm-image-inuse");
    std::fs::create_dir_all(&dir).unwrap();
    let backend = EngineBackend::new(
        &dir,
        "registry.local/pause:0",
        "192.0.2.7",
        "127.0.0.1:15000",
    )
    .unwrap();
    let img = PulledImage {
        reference: "127.0.0.1:15000/test/busy:v1".to_string(),
        digest: "sha256:{}".replace("{}", &"f".repeat(64)),
        config: super::super::image::Descriptor {
            media_type: "cfg".into(),
            digest: "sha256:{}".replace("{}", &"c".repeat(64)),
            size: 1,
            platform: None,
        },
        layers: vec![],
    };
    backend.store.put_ref(&img).unwrap();
    let tmp = backend.store.tmp_file();
    std::fs::write(&tmp, b"x").unwrap();
    backend.store.commit(&tmp, &img.digest).unwrap();

    backend.registry.insert(Entry {
        image: "127.0.0.1:15000/test/busy:v1".to_string(),
        ..sandbox_entry(&dir, "c-inuse", "CONTAINER_RUNNING")
    });
    let err =
        crate::cri_backend::CriBackend::remove_image(&backend, "127.0.0.1:15000/test/busy:v1")
            .await
            .expect_err("in-use image must be refused");
    assert!(err.contains("in use by container c-inuse"), "got: {err}");
    assert!(
        backend.store.has_blob(&img.digest),
        "refused delete keeps the manifest blob"
    );

    backend.registry.remove("c-inuse");
    crate::cri_backend::CriBackend::remove_image(&backend, "127.0.0.1:15000/test/busy:v1")
        .await
        .expect("remove_image after container removal");
    assert!(
        !backend.store.has_blob(&img.digest),
        "released image is swept"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
