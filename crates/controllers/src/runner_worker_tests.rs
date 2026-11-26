//! A3 regression: a panicking reconciler must not kill its worker.
//!
//! Before A3 the worker loop called `reconcile_key` inline, so one panic
//! left the queue forever undrained (fatal for the single-worker kinds:
//! NodeCidr, ServiceLb, Build). The loop now supervises the reconcile as a
//! task: the panic is caught, logged, the key requeued with backoff, and
//! the worker keeps draining.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};
use storage::{EmbeddedStorage, Key, KeyPrefix, StoredEntry, Watch, WatchEvent};

use crate::client::{Client, StorageClient};
use crate::controllers::Caches;
use crate::error::ControllerError;
use crate::runner::Kind;
use crate::runner_worker::spawn_worker;
use crate::stop::Stop;
use crate::workqueue::WorkQueue;

/// Delegates to a real [`StorageClient`], but the FIRST `list` call panics
/// (a reconciler bug) -- every later call behaves normally.
struct PanicOnceClient {
    inner: StorageClient,
    armed: AtomicBool,
    panics: Arc<AtomicUsize>,
}

#[async_trait]
impl Client for PanicOnceClient {
    async fn list(&self, prefix: &KeyPrefix) -> Result<Vec<Value>, ControllerError> {
        if self.armed.swap(false, Ordering::SeqCst) {
            self.panics.fetch_add(1, Ordering::SeqCst);
            panic!("injected reconciler panic (A3 regression)");
        }
        self.inner.list(prefix).await
    }
    async fn get(&self, key: &Key) -> Result<Option<Value>, ControllerError> {
        self.inner.get(key).await
    }
    async fn create(&self, key: &Key, value: Value) -> Result<Value, ControllerError> {
        self.inner.create(key, value).await
    }
    async fn update(
        &self,
        key: &Key,
        value: Value,
        if_revision: Option<u64>,
    ) -> Result<Value, ControllerError> {
        self.inner.update(key, value, if_revision).await
    }
    async fn delete(&self, key: &Key) -> Result<(), ControllerError> {
        self.inner.delete(key).await
    }
    async fn watch(
        &self,
        prefix: &KeyPrefix,
        start_revision: Option<u64>,
    ) -> Result<Watch, ControllerError> {
        self.inner.watch(prefix, start_revision).await
    }
}

fn seed(caches: &Caches, name: &str) {
    let obj = json!({
        "apiVersion": "apps/v1",
        "kind": "Deployment",
        "metadata": {"name": name, "namespace": "default", "uid": format!("uid-{name}")},
        "spec": {"replicas": 1, "template": {"metadata": {"labels": {"app": name}},
                 "spec": {"containers": [{"name": "c", "image": "busybox"}]}}}
    });
    caches
        .deployments
        .apply_event(&WatchEvent::Put(Arc::new(StoredEntry {
            key: format!("/registry/apps/deployments/default/{name}"),
            value: obj,
            create_revision: 1,
            mod_revision: 1,
            version: 1,
        })));
}

#[tokio::test]
async fn a_reconcile_panic_does_not_kill_the_worker() {
    let store = Arc::new(EmbeddedStorage::new());
    let panics = Arc::new(AtomicUsize::new(0));
    let client: Arc<dyn Client> = Arc::new(PanicOnceClient {
        inner: StorageClient::new(store),
        armed: AtomicBool::new(true),
        panics: panics.clone(),
    });
    let caches = Arc::new(Caches::new());
    seed(&caches, "a");
    seed(&caches, "b");

    let queue = Arc::new(WorkQueue::new());
    queue.add("default/a".to_string());
    queue.add("default/b".to_string());

    let stop = Stop::new();
    let worker = spawn_worker(
        &client,
        &caches,
        &queue,
        Kind::Deployment,
        Default::default(),
        &stop,
        Arc::new(|| 0),
    );

    // Both keys must drain: `a` panics once, is requeued with backoff, and
    // its retry (and `b`) then succeed. A dead worker would leave the
    // queue non-empty forever.
    let mut ticks = 0;
    while !queue.is_empty() {
        assert!(
            ticks < 2_000,
            "worker stalled: queue not drained after a panic"
        );
        ticks += 1;
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(
        panics.load(Ordering::SeqCst),
        1,
        "exactly one injected panic"
    );

    // The worker is still alive: a new key drains too.
    seed(&caches, "c");
    queue.add("default/c".to_string());
    let mut ticks = 0;
    while !queue.is_empty() {
        assert!(ticks < 2_000, "worker died after the panic");
        ticks += 1;
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    stop.trigger();
    let _ = worker.await;
}
