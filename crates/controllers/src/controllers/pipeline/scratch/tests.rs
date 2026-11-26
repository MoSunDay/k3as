//! Tests for the pipeline scratch sweeper (TODO **T8.3**, Q56; batch D / B4):
//! the pure planner + cadence rule, and one async pass over a temp root.

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use super::*;

fn sec(n: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(n)
}

/// Old entries go, fresh ones stay, and the TTL edge is exclusive ("older
/// than" the TTL, matching the registry GC convention).
#[test]
fn plan_sweeps_only_entries_older_than_the_ttl() {
    let ttl = Duration::from_secs(3600);
    let entries = vec![
        ("old", sec(10_000 - 3601)),
        ("edge", sec(10_000 - 3600)),
        ("fresh", sec(10_000 - 3599)),
        ("future", sec(10_000 + 1)),
    ]
    .into_iter()
    .map(|(n, t)| (PathBuf::from(n), t))
    .collect();
    assert_eq!(
        sweep_plan(entries, sec(10_000), ttl),
        vec![PathBuf::from("old")]
    );
    assert_eq!(
        sweep_plan(Vec::new(), sec(10_000), ttl),
        Vec::<PathBuf>::new()
    );
}

/// First pass after boot is always due, then spaced by SWEEP_INTERVAL; a
/// clock that went backwards defers.
#[test]
fn cadence_is_due_once_then_spaced() {
    let interval = SWEEP_INTERVAL_MILLIS;
    assert!(sweep_is_due(0, 1));
    assert!(!sweep_is_due(1_000, 1_000 + interval - 1));
    assert!(sweep_is_due(1_000, 1_000 + interval));
    assert!(!sweep_is_due(5_000, 1_000));
}

/// B4: a stale `image.tar` dir and a stale import staging dir are reclaimed,
/// while everything is kept on the real clock; a namespace dir is never
/// removed in the same pass as its children, only once empty and old; the
/// scratch root itself survives; a missing root is a no-op.
#[tokio::test]
async fn sweep_reclaims_stale_scratch_and_empty_namespaces() {
    let root =
        std::env::temp_dir().join(format!("init-pro-pipeline-scratch-{}", std::process::id()));
    let _ = tokio::fs::remove_dir_all(&root).await;
    let build = root.join("default").join("build-crashed");
    let import = root.join("import").join("import-7-0");
    let empty = root.join("empty-ns");
    for d in [&build, &import, &empty] {
        tokio::fs::create_dir_all(d).await.unwrap();
    }
    tokio::fs::write(build.join("image.tar"), b"tar")
        .await
        .unwrap();
    tokio::fs::write(import.join("layer.tar"), b"layer")
        .await
        .unwrap();

    // Real clock: every entry is fresh -> nothing is touched.
    assert_eq!(sweep(&root, SCRATCH_TTL, SystemTime::now()).await, 0);
    assert!(build.join("image.tar").exists());

    // A clock past the TTL makes everything old.
    let later = SystemTime::now() + SCRATCH_TTL + Duration::from_secs(1);
    assert_eq!(
        sweep(&root, SCRATCH_TTL, later).await,
        3,
        "build + import + empty ns"
    );
    assert!(!build.exists());
    assert!(!import.exists());
    assert!(!empty.exists());
    assert!(
        root.join("default").exists(),
        "a namespace emptied by this pass waits"
    );
    assert!(
        root.join("import").exists(),
        "the import root itself is kept"
    );

    // Next pass reclaims the namespace that the previous one emptied.
    assert_eq!(sweep(&root, SCRATCH_TTL, later).await, 1);
    assert!(!root.join("default").exists());
    assert!(root.exists(), "the scratch root is never removed");
    assert_eq!(sweep(&root, SCRATCH_TTL, later).await, 0, "idempotent");
    let _ = tokio::fs::remove_dir_all(&root).await;

    let missing = root.join("never-created");
    assert_eq!(sweep(&missing, SCRATCH_TTL, later).await, 0);
}
