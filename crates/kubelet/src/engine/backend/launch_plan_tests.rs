//! T4.15 (Q60) start-path tests, split out of `backend.rs` at the
//! 800-line rule (same shape as `rm_image_tests.rs`): the host-direct
//! selection routes launch planning BEFORE any image work — a Selected
//! pod plans fine against a bogus image (the image is wire metadata
//! only), a NotSelected pod keeps the byte-compatible Q55 host-mode
//! error, and a broken triple fails create with the HD5 refusal while
//! registering NOTHING.

use super::host_direct_run;
use super::rootfs::ROOTFS_ANNOTATION;
use super::tests::{sandbox_entry, temp_data_dir};
use super::*;
use runtime::cri_json::{ContainerMetadata, ImageSpec, SandboxMetadata};
use runtime::{ContainerConfig, PodSandboxConfig};

const BOGUS_IMAGE: &str = "registry.invalid/no-such/image:never";

fn backend(tag: &str, mode: RootfsMode) -> EngineBackend {
    let dir = temp_data_dir(tag);
    std::fs::create_dir_all(&dir).unwrap();
    let backend = EngineBackend::new(
        &dir,
        "registry.local/pause:0",
        "192.0.2.7",
        "127.0.0.1:15000",
    )
    .unwrap()
    .with_rootfs_mode(mode);
    let _ = std::fs::remove_dir_all(&dir);
    backend
}

fn pair(annotations: &[(&str, &str)]) -> (ContainerConfig, PodSandboxConfig) {
    (
        ContainerConfig {
            metadata: ContainerMetadata {
                name: "infer".into(),
                attempt: 0,
            },
            image: ImageSpec {
                image: BOGUS_IMAGE.into(),
            },
            command: vec!["/usr/bin/true".into()],
            log_path: "infer.0.log".into(),
            ..Default::default()
        },
        PodSandboxConfig {
            metadata: SandboxMetadata {
                name: "p-hd".into(),
                namespace: "default".into(),
                uid: "u".into(),
                attempt: 0,
            },
            annotations: annotations
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            ..Default::default()
        },
    )
}

fn pending_for(annotations: &[(&str, &str)]) -> Pending {
    let (ccfg, pcfg) = pair(annotations);
    Pending {
        ccfg,
        pcfg,
        sandbox_id: "sb-1".into(),
    }
}

/// HD2 structural proof, backend side: a Selected container plans with a
/// bogus image — no resolve/pull/presence/image-config step is reachable
/// on the host-direct path (the old chain answers "no root directory").
#[tokio::test]
async fn selected_container_plans_with_a_bogus_image() {
    let backend = backend("hd-selected", RootfsMode::Host);
    let pending = pending_for(&[(ROOTFS_ANNOTATION, "/")]);
    assert!(host_direct_run::selected(backend.rootfs_mode, &pending.pcfg.annotations).unwrap());
    let es = host_direct_run::launch_spec("c-hd", &pending).expect("image must not matter");
    assert!(es.host_root && es.netns.is_none());
}

/// NotSelected keeps the Q55/R1 host-mode error byte-compatible: a bogus
/// image with NO annotation still fails at the image chain (after the
/// same sandbox-netns readiness check as before T4.15).
#[tokio::test]
async fn not_selected_keeps_the_host_mode_missing_image_error() {
    let dir = temp_data_dir("hd-not-selected");
    std::fs::create_dir_all(&dir).unwrap();
    let backend = EngineBackend::new(
        &dir,
        "registry.local/pause:0",
        "192.0.2.7",
        "127.0.0.1:15000",
    )
    .unwrap()
    .with_rootfs_mode(RootfsMode::Host);
    backend
        .registry
        .insert(sandbox_entry(&dir, "sb-1", "SANDBOX_READY"));
    let pending = pending_for(&[]);
    assert!(!host_direct_run::selected(backend.rootfs_mode, &pending.pcfg.annotations).unwrap());
    let err = backend.launch_plan("c-x", &pending).await.unwrap_err();
    assert!(err.contains("no root directory for image"), "{err}");
    assert!(!err.contains("host-direct"), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// HD5: a broken triple (literal `/` on an image-mode engine) fails
/// `create_container` with the actionable refusal and registers NOTHING
/// — no partial container, no image work, no containerd fallback.
#[tokio::test]
async fn invalid_selection_fails_create_and_registers_nothing() {
    let backend = backend("hd-invalid", RootfsMode::Image);
    let (ccfg, pcfg) = pair(&[(ROOTFS_ANNOTATION, "/")]);
    let err = backend
        .create_container("sb-1", &ccfg, &pcfg)
        .await
        .expect_err("broken triple must refuse");
    assert!(err.contains("host-direct refused"), "{err}");
    assert!(err.contains("--builtin-rootfs=host"), "{err}");
    assert!(
        backend.registry.container_views().is_empty(),
        "nothing registered"
    );
}

/// The happy create gate: a Selected container registers its pending
/// config even with a bogus image (image bytes are never consulted at
/// create; the start gate plans host-direct).
#[tokio::test]
async fn selected_container_registers_pending_with_bogus_image() {
    let backend = backend("hd-create", RootfsMode::Host);
    let (ccfg, pcfg) = pair(&[(ROOTFS_ANNOTATION, "/")]);
    let id = backend
        .create_container("sb-1", &ccfg, &pcfg)
        .await
        .expect("create");
    let pending = backend.registry.pending(&id).expect("pending registered");
    assert_eq!(pending.ccfg.image.image, BOGUS_IMAGE);
    assert!(
        backend.registry.netns_of("sb-1").is_none(),
        "sandbox not staged"
    );
    // With no sandbox entry the Q55 path would fail "sandbox not ready";
    // the host-direct plan does not consult the sandbox netns at all.
    assert!(host_direct_run::launch_spec(&id, &pending).is_ok());
}
