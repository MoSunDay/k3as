//! H2/H4 (C14) pure planner cases for `agent_sandbox_plan.rs` plus the
//! dir sweep of `agent_sandbox.rs` (file size limit; the `gpu_tests.rs`
//! precedent). Temp dirs are pid-scoped and removed at the end.

use std::path::Path;
use std::time::Duration;

use serde_json::json;

use super::*;
use crate::agent_sandbox::{prepare_sandbox_dirs, remove_attempt_dirs, sweep_stale_attempt_dirs};

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("ip-sbx-plan-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// H2 (C14): 500 ms doubling per consecutive failure, capped at 5 min;
/// the curve is monotonic and `0` failures behaves like the first one.
#[test]
fn retry_delay_doubles_from_500ms_to_a_5min_cap() {
    assert_eq!(retry_delay(1), Duration::from_millis(500));
    assert_eq!(retry_delay(2), Duration::from_secs(1));
    assert_eq!(retry_delay(3), Duration::from_secs(2));
    assert_eq!(retry_delay(4), Duration::from_secs(4));
    assert_eq!(retry_delay(7), Duration::from_secs(32));
    assert_eq!(retry_delay(10), Duration::from_secs(256), "still doubling");
    assert_eq!(retry_delay(11), Duration::from_secs(300), "capped");
    assert_eq!(retry_delay(u32::MAX), Duration::from_secs(300));
    assert_eq!(retry_delay(0), Duration::from_millis(500));
    let mut prev = Duration::ZERO;
    for n in 1..40 {
        let d = retry_delay(n);
        assert!(d >= prev, "monotonic at {n}");
        assert!(d <= Duration::from_secs(300));
        prev = d;
    }
}

/// H4 (C14): the dir layout matches `prepare_sandbox_dirs` and the id
/// goes through the same sanitize rules as the engine socket name.
#[test]
fn attempt_dir_is_sanitized_and_matches_prepare() {
    let base = Path::new("/data");
    assert_eq!(
        attempt_dir(base, "ag-1-ab12"),
        Path::new("/data/agent-sandbox/ag-1-ab12")
    );
    assert_eq!(
        attempt_dir(base, "a/b c"),
        Path::new("/data/agent-sandbox/a-b-c")
    );
    let (subdirs, dir) = attempt_cleanup_paths(base, "x");
    assert_eq!(
        subdirs,
        vec![
            Path::new("/data/agent-sandbox/x/rootfs"),
            Path::new("/data/agent-sandbox/x/scratch")
        ]
    );
    assert_eq!(dir, Path::new("/data/agent-sandbox/x"));
    // prepare_sandbox_dirs builds exactly the planned tree.
    let data = temp_dir("layout");
    let (rootfs, scratch) = prepare_sandbox_dirs(&data, "ag-1").unwrap();
    let (subdirs, dir) = attempt_cleanup_paths(&data, "ag-1");
    assert_eq!((rootfs, scratch), (subdirs[0].clone(), subdirs[1].clone()));
    assert!(dir.is_dir());
    std::fs::remove_dir_all(&data).unwrap();
}

/// H4 (C14): everything found without a live socket is stale (sorted,
/// deduplicated); live ids are kept; an empty found list plans nothing.
#[test]
fn stale_attempt_dirs_plan_keeps_only_live_sockets() {
    let found = vec![
        "c".to_string(),
        "a".to_string(),
        "b".to_string(),
        "a".to_string(),
    ];
    let live = vec!["b".to_string(), "not-found".to_string()];
    assert_eq!(stale_attempt_dirs(&found, &live), vec!["a", "c"]);
    assert!(stale_attempt_dirs(&[], &["x".to_string()]).is_empty());
    assert_eq!(
        stale_attempt_dirs(&found, &[]),
        vec!["a", "b", "c"],
        "nothing live: everything is stale, deduplicated"
    );
    let all = vec!["a".to_string(), "b".to_string(), "c".to_string()];
    assert!(stale_attempt_dirs(&all, &all).is_empty());
}

/// H4 (C14): the sweep removes attempt dirs whose `<id>.sock` is absent
/// (and whatever the previous kubelet staged in them), keeps the ones
/// whose socket still exists, ignores plain files, and is idempotent.
#[test]
fn sweep_removes_only_attempt_dirs_without_a_socket() {
    let data = temp_dir("sweep");
    for id in ["gone-1", "gone-2", "live-1"] {
        prepare_sandbox_dirs(&data, id).unwrap();
        let (rootfs, _) = prepare_sandbox_dirs(&data, id).unwrap();
        std::fs::write(rootfs.join("runner"), b"staged").unwrap();
    }
    let root = data.join(AGENT_SANDBOX_DIR);
    std::fs::write(root.join("stray-file"), b"x").unwrap();
    let socks = crate::engine::launch::engine_dir(&data);
    std::fs::create_dir_all(&socks).unwrap();
    std::fs::write(socks.join("live-1.sock"), b"").unwrap();
    assert_eq!(sweep_stale_attempt_dirs(&data), 2);
    assert!(!root.join("gone-1").exists());
    assert!(!root.join("gone-2").exists());
    assert!(
        root.join("live-1/rootfs/runner").is_file(),
        "live attempt untouched"
    );
    assert!(
        root.join("stray-file").is_file(),
        "non-dirs are not attempt dirs"
    );
    assert_eq!(sweep_stale_attempt_dirs(&data), 0, "idempotent");
    // No sandbox ever ran here: nothing to do, no error.
    assert_eq!(sweep_stale_attempt_dirs(&data.join("elsewhere")), 0);
    std::fs::remove_dir_all(&data).unwrap();
}

/// H4 (C14): `remove_attempt_dirs` deletes rootfs/scratch recursively and
/// the attempt dir only once empty; a sibling parked next to them keeps
/// the parent (reported as "not fully removed", never an error).
#[test]
fn remove_attempt_dirs_keeps_unknown_siblings() {
    let data = temp_dir("remove");
    prepare_sandbox_dirs(&data, "ag-1").unwrap();
    let (rootfs, scratch) = prepare_sandbox_dirs(&data, "ag-1").unwrap();
    std::fs::create_dir_all(rootfs.join("runner")).unwrap();
    std::fs::write(rootfs.join("runner").join("bin"), b"x").unwrap();
    std::fs::write(scratch.join("note"), b"x").unwrap();
    assert!(remove_attempt_dirs(&data, "ag-1"));
    assert!(!data.join(AGENT_SANDBOX_DIR).join("ag-1").exists());
    // Unknown sibling: the parent survives, rootfs/scratch still go.
    prepare_sandbox_dirs(&data, "ag-2").unwrap();
    std::fs::write(
        data.join(AGENT_SANDBOX_DIR).join("ag-2").join("ops"),
        b"keep",
    )
    .unwrap();
    assert!(
        !remove_attempt_dirs(&data, "ag-2"),
        "parent kept for the sibling"
    );
    let (subdirs, _) = attempt_cleanup_paths(&data, "ag-2");
    assert!(subdirs.iter().all(|p| !p.exists()));
    assert!(data
        .join(AGENT_SANDBOX_DIR)
        .join("ag-2")
        .join("ops")
        .is_file());
    // Absent attempt: a no-op that reports success.
    assert!(remove_attempt_dirs(&data, "never-ran"));
    std::fs::remove_dir_all(&data).unwrap();
}

/// H2 (C14): `pending_reason` reads the Ready condition of a Pending
/// body and refuses every other shape.
#[test]
fn pending_reason_reads_only_pending_ready_bodies() {
    let pending = objs::status_patch(
        objs::PHASE_PENDING,
        &[objs::condition(
            objs::COND_READY,
            "False",
            "SandboxUnavailable",
            "x",
        )],
    );
    assert_eq!(pending_reason(&pending), Some("SandboxUnavailable"));
    let failed = objs::status_patch(
        objs::PHASE_FAILED,
        &[objs::condition(
            objs::COND_READY,
            "False",
            "RunnerFailed",
            "x",
        )],
    );
    assert_eq!(pending_reason(&failed), None);
    assert_eq!(pending_reason(&json!({})), None);
    assert_eq!(
        pending_reason(&json!({"status": {"phase": "Pending"}})),
        None
    );
}
