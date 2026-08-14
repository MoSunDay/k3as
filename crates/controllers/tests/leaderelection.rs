//! Leader-election integration tests (T3.1a, Q18): Lease + resourceVersion
//! CAS against `EmbeddedStorage`, with a MANUAL clock only (no timing
//! windows anywhere in these tests).

use std::sync::{Arc, Mutex};

use controllers::{Client, LeaderElector, LeaseConfig, StorageClient};
use serde_json::Value;
use storage::{EmbeddedStorage, Key, StorageBackend};

type Clock = Arc<Mutex<u64>>;

fn cfg_for(clock: &Clock, identity: &str) -> LeaseConfig {
    let c = clock.clone();
    LeaseConfig {
        namespace: "kube-system".into(),
        name: "init-pro-controller-manager".into(),
        identity: identity.into(),
        lease_duration: 30,
        retry_period: 5, // ms; irrelevant to the assertions (manual clock)
        now: Arc::new(move || *c.lock().unwrap()),
    }
}

fn lease_key() -> Key {
    Key::new(
        "coordination.k8s.io",
        "leases",
        "kube-system",
        "init-pro-controller-manager",
    )
}

async fn lease_of(client: &StorageClient) -> Value {
    client
        .get(&lease_key())
        .await
        .unwrap()
        .expect("lease exists")
}

#[tokio::test]
async fn first_candidate_acquires_and_renew_skips_fresh_writes() {
    let store = Arc::new(EmbeddedStorage::new());
    let client = StorageClient::new(store.clone());
    let clock: Clock = Arc::new(Mutex::new(1000));
    let a = LeaderElector::new(Arc::new(client.clone()), cfg_for(&clock, "host-a"));

    assert!(
        a.try_acquire_or_renew().await.unwrap(),
        "A acquires the fresh lease"
    );
    let lease = lease_of(&client).await;
    assert_eq!(lease["spec"]["holderIdentity"], "host-a");
    assert_eq!(lease["spec"]["leaseTransitions"], 0);
    assert_eq!(lease["metadata"]["namespace"], "kube-system");

    // While the lease is fresh (< lease_duration/2 elapsed) a renew reports
    // leadership WITHOUT writing (revision-stability contract).
    let rev_before = store.current_revision().await.unwrap();
    assert!(a.try_acquire_or_renew().await.unwrap());
    assert_eq!(
        store.current_revision().await.unwrap(),
        rev_before,
        "fresh-lease renew must not bump the revision"
    );
}

#[tokio::test]
async fn second_candidate_cannot_steal_a_fresh_lease() {
    let store = Arc::new(EmbeddedStorage::new());
    let client = StorageClient::new(store.clone());
    let clock: Clock = Arc::new(Mutex::new(1000));
    let a = LeaderElector::new(Arc::new(client.clone()), cfg_for(&clock, "host-a"));
    let b = LeaderElector::new(Arc::new(client.clone()), cfg_for(&clock, "host-b"));
    assert!(a.try_acquire_or_renew().await.unwrap());

    // Only 10s of the 30s lease elapsed: B must not acquire.
    *clock.lock().unwrap() = 1010;
    assert!(
        !b.try_acquire_or_renew().await.unwrap(),
        "fresh lease is not stealable"
    );
    assert_eq!(lease_of(&client).await["spec"]["holderIdentity"], "host-a");
}

#[tokio::test]
async fn expired_lease_is_taken_over_with_transition_count() {
    let store = Arc::new(EmbeddedStorage::new());
    let client = StorageClient::new(store.clone());
    let clock: Clock = Arc::new(Mutex::new(1000));
    let a = LeaderElector::new(Arc::new(client.clone()), cfg_for(&clock, "host-a"));
    let b = LeaderElector::new(Arc::new(client.clone()), cfg_for(&clock, "host-b"));
    assert!(a.try_acquire_or_renew().await.unwrap());

    // Advance PAST lease_duration: B takes over, transitions bump to 1.
    *clock.lock().unwrap() = 1100;
    assert!(
        b.try_acquire_or_renew().await.unwrap(),
        "expired lease is takeable"
    );
    let lease = lease_of(&client).await;
    assert_eq!(lease["spec"]["holderIdentity"], "host-b");
    assert_eq!(lease["spec"]["leaseTransitions"], 1);
    // A lost leadership: B's fresh lease blocks A's re-acquire attempt.
    assert!(
        !a.try_acquire_or_renew().await.unwrap(),
        "deposed holder cannot re-acquire"
    );
}

#[tokio::test]
async fn spawned_elector_reports_leadership_on_watch_channel() {
    let store = Arc::new(EmbeddedStorage::new());
    let client: Arc<dyn Client> = Arc::new(StorageClient::new(store.clone()));
    let clock: Clock = Arc::new(Mutex::new(1000));
    let elector = LeaderElector::new(client, cfg_for(&clock, "host-a"));
    let stop = controllers::Stop::new();
    let (handle, mut rx) = elector.spawn(stop.clone());

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while !*rx.borrow_and_update() {
        assert!(
            std::time::Instant::now() < deadline,
            "leadership never reported"
        );
        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_millis(5)) => {}
            r = rx.changed() => { r.expect("elector alive"); }
        }
    }
    stop.trigger();
    tokio::time::timeout(std::time::Duration::from_secs(1), handle)
        .await
        .unwrap()
        .unwrap();
    assert!(!*rx.borrow(), "stop forces the leadership signal low");
}
