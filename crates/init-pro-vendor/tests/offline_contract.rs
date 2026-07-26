//! Offline-contract integration tests for the acquire pipeline (T0.2, Q6).
//!
//! Exercises the public `run()` API with a temp vendor dir and no network:
//! the air-gap mode must fail on a missing/unverified artifact, while the
//! default Auto mode must skip without error.

#![forbid(unsafe_code)]

use init_pro_vendor::{run, AcquireError, Artifact, Kind, Mode};
use std::path::PathBuf;

fn fake_artifact() -> Artifact {
    Artifact {
        name: "fake".into(),
        version: "1.0.0".into(),
        license: "Apache-2.0".into(),
        url: "https://example.invalid/fake".into(),
        sha256: "0000000000000000000000000000000000000000000000000000000000000000".into(),
        kind: Kind::Bin,
        into: ".".into(),
        strip: 0,
        install_as: None,
    }
}

fn fresh_vendor(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("initpro-vendor-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

#[test]
fn offline_empty_cache_fails() {
    let root = fresh_vendor("empty");
    let err = run(&root, &[fake_artifact()], Mode::Offline).unwrap_err();
    assert!(matches!(err, AcquireError::OfflineMissing { .. }), "{err:?}");
    assert!(err.hint().contains("INIT_PRO_VENDOR"), "hint missing fix: {err}");
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn auto_empty_cache_skips() {
    let root = fresh_vendor("skip");
    let rep = run(&root, &[fake_artifact()], Mode::Auto).unwrap();
    assert_eq!(rep.skipped, 1);
    assert_eq!(rep.downloaded, 0);
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn offline_wrong_sha_treated_as_missing() {
    // A cache file whose SHA does not match the pin must NOT satisfy offline
    // mode — only verified copies count toward the air-gap contract.
    let root = fresh_vendor("badsha");
    std::fs::create_dir_all(root.join("cache")).unwrap();
    std::fs::write(root.join("cache").join("fake"), b"corrupt-or-wrong").unwrap();
    let err = run(&root, &[fake_artifact()], Mode::Offline).unwrap_err();
    assert!(matches!(err, AcquireError::OfflineMissing { .. }), "{err:?}");
    std::fs::remove_dir_all(&root).ok();
}
