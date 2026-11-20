//! Ledger unit tests (GPU2/GPU3/GPU4; TODO **T7.2**, decision
//! **Q61**). Extracted from `ledger.rs` to keep both files inside
//! the 400-line new-file budget.

use super::*;

fn totals(nv: u64) -> BTreeMap<String, u64> {
    BTreeMap::from([("nvidia.com/gpu".to_string(), nv)])
}

fn want(container: &str, count: u64) -> Want {
    (container.to_string(), "nvidia.com/gpu".to_string(), count)
}

#[test]
fn gpu2_exclusive_allocation_and_exhaustion() {
    let mut l = GpuLedger {
        path: PathBuf::from("/tmp/unused"),
        state: LedgerState::default(),
    };
    let a = l.allocate_pod("uid-a", &[want("c", 1)], &totals(1), 100);
    assert_eq!(a.unwrap(), BTreeMap::from([("c".to_string(), vec![0])]));
    // Second pod, one device universe: exhausted -> actionable error.
    let b = l.allocate_pod("uid-b", &[want("c", 1)], &totals(1), 101);
    assert!(b.is_err());
    assert!(b.unwrap_err().contains("insufficient free nvidia.com/gpu"));
    // Structurally: device 0 lives in exactly one entry.
    assert_eq!(l.allocated_count("nvidia.com/gpu"), 1);
}

#[test]
fn gpu2_two_devices_two_pods_then_double_book_rejected() {
    let mut l = GpuLedger {
        path: PathBuf::from("/tmp/unused"),
        state: LedgerState::default(),
    };
    l.allocate_pod("uid-a", &[want("c", 1)], &totals(2), 100)
        .unwrap();
    let b = l
        .allocate_pod("uid-b", &[want("c", 1)], &totals(2), 101)
        .unwrap();
    assert_eq!(b, BTreeMap::from([("c".to_string(), vec![1])]));
    let c = l.allocate_pod("uid-c", &[want("c", 1)], &totals(2), 102);
    assert!(c.is_err());
    assert!(l.free_devices("nvidia.com/gpu", 2).is_empty());
}

#[test]
fn gpu3_same_uid_container_keeps_devices_across_retries() {
    let mut l = GpuLedger {
        path: PathBuf::from("/tmp/unused"),
        state: LedgerState::default(),
    };
    let first = l
        .allocate_pod("uid", &[want("c", 1)], &totals(4), 100)
        .unwrap();
    // Later free devices exist (1,2,3) but the retry must keep device 0.
    let again = l
        .allocate_pod("uid", &[want("c", 1)], &totals(4), 200)
        .unwrap();
    assert_eq!(first, again);
    assert_eq!(again["c"], vec![0]);
}

#[test]
fn gpu3_restart_reloads_ledger_from_disk() {
    let dir = std::env::temp_dir().join(format!("gpu-ledger-{}", std::process::id()));
    let path = dir.join("engine/gpu-ledger.json");
    let _ = std::fs::remove_file(&path);
    {
        let mut l = GpuLedger::open(&path);
        l.allocate_pod("uid", &[want("c", 2)], &totals(2), 100)
            .unwrap();
        l.save().unwrap();
    }
    // Agent restart: same mapping preserved, still exclusive.
    let re = GpuLedger::open(&path);
    assert_eq!(
        re.entry("uid", "c")
            .map(|e| (e.resource.as_str(), e.devices.as_slice())),
        Some(("nvidia.com/gpu", &[0u64, 1][..]))
    );
    let mut re = re;
    let again = re
        .allocate_pod("uid", &[want("c", 2)], &totals(2), 999)
        .unwrap();
    assert_eq!(again["c"], vec![0, 1]);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn gpu4_release_only_after_teardown_then_reusable() {
    let mut l = GpuLedger {
        path: PathBuf::from("/tmp/unused"),
        state: LedgerState::default(),
    };
    l.allocate_pod("uid-a", &[want("c", 1)], &totals(1), 100)
        .unwrap();
    // Deletion alone frees nothing: a competing pod still cannot book.
    assert!(l
        .allocate_pod("uid-b", &[want("c", 1)], &totals(1), 101)
        .is_err());
    assert!(l.release("uid-a", 200, "teardown-complete"));
    // After teardown the device is reusable by the next pod.
    let b = l
        .allocate_pod("uid-b", &[want("c", 1)], &totals(1), 201)
        .unwrap();
    assert_eq!(b["c"], vec![0]);
    // The audit record landed with the exact contract reason.
    assert_eq!(l.state.released.len(), 1);
    assert_eq!(l.state.released[0].reason, "teardown-complete");
    assert_eq!(l.state.released[0].pod_uid, "uid-a");
    assert_eq!(l.state.released[0].at, common::time::now_rfc3339(200));
    // Idempotent: releasing again is a no-op (crash/retry safe).
    assert!(!l.release("uid-a", 202, "teardown-complete"));
}

#[test]
fn gpu4_reconcile_grace_keeps_fresh_absent_prunes_old() {
    let mut l = GpuLedger {
        path: PathBuf::from("/tmp/unused"),
        state: LedgerState::default(),
    };
    l.allocate_pod("uid-gone", &[want("c", 1)], &totals(1), 1000)
        .unwrap();
    let live: BTreeSet<String> = BTreeSet::new();
    // Inside the grace window: kept (crash/retry loops don't leak).
    assert!(!l.reconcile(&live, 1000 + RECONCILE_GRACE_SECS));
    assert!(l.entry("uid-gone", "c").is_some());
    // Past the grace: pruned as a released record.
    assert!(l.reconcile(&live, 1000 + RECONCILE_GRACE_SECS + 1));
    assert!(l.entry("uid-gone", "c").is_none());
    assert_eq!(l.state.released[0].reason, "pod-absent-grace");
    // A live pod is never pruned regardless of age.
    l.allocate_pod("uid-live", &[want("c", 1)], &totals(1), 0)
        .unwrap();
    let live: BTreeSet<String> = BTreeSet::from(["uid-live".to_string()]);
    assert!(!l.reconcile(&live, 10_000_000));
    assert!(l.entry("uid-live", "c").is_some());
}

#[test]
fn gpu4_released_records_pruned_to_ten_minutes() {
    let mut l = GpuLedger {
        path: PathBuf::from("/tmp/unused"),
        state: LedgerState::default(),
    };
    l.allocate_pod("uid-old", &[want("c", 1)], &totals(1), 0)
        .unwrap();
    l.release("uid-old", 100, "teardown-complete");
    l.allocate_pod("uid-new", &[want("c", 1)], &totals(1), 200)
        .unwrap();
    l.release("uid-new", 300, "teardown-complete");
    assert_eq!(l.state.released.len(), 2);
    // 300 + retention keeps only the newest record.
    assert!(l.prune_released(300 + RELEASED_RETENTION_SECS));
    assert_eq!(l.state.released.len(), 1);
    assert_eq!(l.state.released[0].pod_uid, "uid-new");
}

#[test]
fn gpu2_pod_allocation_is_transactional() {
    let mut l = GpuLedger {
        path: PathBuf::from("/tmp/unused"),
        state: LedgerState::default(),
    };
    l.allocate_pod("uid-a", &[want("c", 1)], &totals(1), 100)
        .unwrap();
    // Two wants, only one device: the WHOLE pod fails and the first
    // want's partial entry is rolled back (no leaked devices).
    let r = l.allocate_pod("uid-b", &[want("c1", 1), want("c2", 1)], &totals(2), 101);
    // uid-a holds 0; c1 took 1; c2 has nothing free -> error.
    assert!(r.is_err());
    assert!(l
        .state
        .allocations
        .get("uid-b")
        .is_none_or(|m| m.is_empty()));
    assert_eq!(l.allocated_count("nvidia.com/gpu"), 1);
    // A single-container pod can still take the remaining device.
    assert!(l
        .allocate_pod("uid-c", &[want("c", 1)], &totals(2), 102)
        .is_ok());
}

#[test]
fn gpu2_unknown_resource_rejected() {
    let mut l = GpuLedger {
        path: PathBuf::from("/tmp/unused"),
        state: LedgerState::default(),
    };
    let r = l.allocate_pod(
        "uid",
        &[("c".to_string(), "cpu".to_string(), 1)],
        &BTreeMap::new(),
        0,
    );
    assert!(r.unwrap_err().contains("unknown GPU resource"));
}
