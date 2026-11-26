//! H2/H3 (C14) run-loop guard cases split out of `agents.rs` (file size
//! limit): the `killers` slot drop guard and the pure Pending backoff.

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::json;
use tokio::sync::Mutex;

use super::*;

/// `prompt` stands in for "the spec": the backoff stamp keys on it.
fn record(key: &str, uid: &str, prompt: &str) -> AgentRecord {
    let raw = json!({
        "metadata": {"name": key.split('/').nth(1).unwrap_or(key),
                     "namespace": key.split('/').next().unwrap_or("default"),
                     "uid": uid, "resourceVersion": "1"},
        "spec": {"prompt": prompt},
        "status": {"phase": "Pending"}
    });
    AgentRecord {
        view: objs::agent_view(&raw).unwrap(),
        raw,
    }
}

/// H3 (C14): the normal path -- the slot exists while the guard lives and
/// is gone once it drops; the receiver is told to stop (sender closed).
#[tokio::test]
async fn slot_guard_releases_the_killers_slot_on_drop() {
    let killers: Killers = Arc::new(Mutex::new(BTreeMap::new()));
    let (slot, mut rx) = SlotGuard::acquire(Arc::clone(&killers), "default/ag-1").await;
    assert!(killers.lock().await.contains_key("default/ag-1"));
    assert!(*rx.borrow(), "keep going while the slot is live");
    drop(slot);
    assert!(!killers.lock().await.contains_key("default/ag-1"));
    assert!(rx.changed().await.is_err(), "sender dropped => stop");
    // A second release is a no-op (no double free shape): the map stays
    // empty and the inline path simply reports "the lock was taken".
    release_slot(&killers, "default/ag-1");
    assert!(!killers.lock().await.contains_key("default/ag-1"));
}

/// H3 (C14): the bug this guards against -- `run_attempt` panics inside
/// the spawned task. The slot must still be released (the loop can
/// re-spawn the Agent) instead of pinning it out of the scan forever.
#[tokio::test]
async fn slot_guard_releases_the_slot_when_the_attempt_panics() {
    let killers: Killers = Arc::new(Mutex::new(BTreeMap::new()));
    let task = {
        let killers = Arc::clone(&killers);
        tokio::spawn(async move {
            let (slot, _rx) = SlotGuard::acquire(killers, "default/ag-2").await;
            let _live = &slot; // the guard is alive when the panic hits
            panic!("run_attempt exploded (test)");
        })
    };
    assert!(task.await.is_err(), "the panic is contained by the task");
    let deadline = Instant::now() + Duration::from_secs(2);
    while killers.lock().await.contains_key("default/ag-2") {
        assert!(Instant::now() < deadline, "slot leaked after a panic");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// H3 (C14): a cancelled task (runtime shutdown dropping the future
/// mid-await) drops the guard too.
#[tokio::test]
async fn slot_guard_releases_the_slot_when_the_task_is_cancelled() {
    let killers: Killers = Arc::new(Mutex::new(BTreeMap::new()));
    let task = {
        let killers = Arc::clone(&killers);
        tokio::spawn(async move {
            let (slot, _rx) = SlotGuard::acquire(killers, "default/ag-3").await;
            std::future::pending::<()>().await;
            drop(slot);
        })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(killers.lock().await.contains_key("default/ag-3"));
    task.abort();
    let _ = task.await;
    let mut gone = false;
    for _ in 0..200 {
        if !killers.lock().await.contains_key("default/ag-3") {
            gone = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(gone, "slot leaked after task cancellation");
}

/// H3 (C14): the contended fallback -- the lock is held across the drop,
/// so `release_slot` cannot spin to success and defers to the runtime.
#[tokio::test]
async fn slot_guard_defers_release_when_the_lock_is_held() {
    let killers: Killers = Arc::new(Mutex::new(BTreeMap::new()));
    let (slot, _rx) = SlotGuard::acquire(Arc::clone(&killers), "default/ag-4").await;
    assert!(killers.lock().await.contains_key("default/ag-4"));
    let held = killers.clone().lock_owned().await; // contended from here on
    drop(slot); // the spin fails: the guard defers to the runtime
    assert!(killers.try_lock().is_err(), "still held by the test");
    drop(held);
    let mut gone = false;
    for _ in 0..500 {
        if !killers.lock().await.contains_key("default/ag-4") {
            gone = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(gone, "the deferred async removal never ran");
}

/// H2 (C14): the stamp follows the uid AND the spec, never the status (the
/// runner's own Pending write must not reset the curve); the backoff is
/// active only for the same object inside its window; each failure adds
/// one step on the curve and a spec-edited/recreated object starts over.
#[test]
fn backoff_is_per_object_and_grows_with_each_failure() {
    let a1 = record("default/ag-1", "uid-1", "do it");
    let a2 = record("default/ag-1", "uid-1", "do it, fixed");
    let b = record("default/ag-1", "uid-2", "do it");
    let stamp = object_stamp(&a1.raw);
    assert!(stamp.starts_with("uid-1#{"), "{stamp}");
    assert!(stamp.contains("do it"), "{stamp}");
    // A status write (phase/conditions/resourceVersion bump) keeps it.
    let mut patched = a1.raw.clone();
    patched["status"] = json!({"phase": "Pending", "conditions": []});
    patched["metadata"]["resourceVersion"] = json!("2");
    assert_eq!(
        object_stamp(&patched),
        stamp,
        "status writes keep the stamp"
    );
    assert_ne!(object_stamp(&a2.raw), stamp, "spec edit: new stamp");
    assert_ne!(object_stamp(&b.raw), stamp, "recreated: new stamp");

    let t0 = Instant::now();
    assert!(!backoff_active(None, &stamp, t0));
    let first = backoff_after_failure(None, &stamp, t0);
    assert_eq!((first.failures, first.object.clone()), (1, stamp.clone()));
    assert_eq!(first.next_retry, t0 + Duration::from_millis(500));
    assert!(backoff_active(
        Some(&first),
        &stamp,
        t0 + Duration::from_millis(499)
    ));
    assert!(!backoff_active(
        Some(&first),
        &stamp,
        t0 + Duration::from_millis(500)
    ));
    assert!(
        !backoff_active(Some(&first), &object_stamp(&a2.raw), t0),
        "edited object: reset"
    );
    assert!(
        !backoff_active(Some(&first), &object_stamp(&b.raw), t0),
        "recreated object: reset"
    );

    let second = backoff_after_failure(Some(&first), &stamp, t0);
    assert_eq!(second.next_retry, t0 + Duration::from_secs(1));
    let restarted = backoff_after_failure(Some(&first), &object_stamp(&a2.raw), t0);
    assert_eq!(restarted.failures, 1);
    let capped = (1..=12).fold(None, |prev: Option<PendingBackoff>, _| {
        Some(backoff_after_failure(prev.as_ref(), &stamp, t0))
    });
    assert_eq!(capped.unwrap().next_retry, t0 + Duration::from_secs(300));
}

/// H2 (C14): entries of vanished objects are pruned; live ones stay.
#[tokio::test]
async fn prune_backoff_drops_vanished_objects() {
    let backoff: Backoff = Arc::new(Mutex::new(BTreeMap::new()));
    let now = Instant::now();
    for key in ["default/ag-1", "default/ag-2", "default/ag-3"] {
        backoff
            .lock()
            .await
            .insert(key.to_string(), backoff_after_failure(None, "uid#1", now));
    }
    let snapshot = vec![record("default/ag-2", "uid", "do it")];
    prune_backoff(&backoff, &snapshot).await;
    let keys: Vec<String> = backoff.lock().await.keys().cloned().collect();
    assert_eq!(keys, vec!["default/ag-2".to_string()]);
    assert_eq!(retry_delay(1), Duration::from_millis(500), "curve wired in");
}
