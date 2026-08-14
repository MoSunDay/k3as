//! Informer integration tests (T3.1a): LIST seeding, live seam, deletes,
//! and watch-close -> re-list, against `EmbeddedStorage` + `StorageClient`.
//! Handler expectations use mpsc channels inside `tokio::time::timeout`.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use controllers::{Client, ControllerError, EventHandler, Informer, ObjectStore, Stop};
use serde_json::{json, Value};
use storage::{EmbeddedStorage, Key, KeyPrefix, StorageBackend, WatchEvent};

fn pod(name: &str) -> Value {
    json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {"name": name, "namespace": "default", "labels": {"app": "x"}},
        "spec": {"containers": [{"name": "c", "image": "nginx"}]},
    })
}

async fn seed(store: &EmbeddedStorage, name: &str) -> u64 {
    store
        .create(&Key::new("", "pods", "default", name), pod(name))
        .await
        .unwrap()
        .mod_revision
}

fn handlers() -> (
    EventHandler,
    tokio::sync::mpsc::UnboundedReceiver<WatchEvent>,
) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    (
        Arc::new(move |ev: &WatchEvent| {
            tx.send(ev.clone()).ok();
        }),
        rx,
    )
}

async fn expect_put(rx: &mut tokio::sync::mpsc::UnboundedReceiver<WatchEvent>, name: &str) {
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("timed out waiting for event")
            .expect("handler dropped");
        if let WatchEvent::Put(e) = ev {
            if e.value.pointer("/metadata/name").and_then(Value::as_str) == Some(name) {
                return;
            }
        }
    }
}

async fn expect_delete(rx: &mut tokio::sync::mpsc::UnboundedReceiver<WatchEvent>, name: &str) {
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("timed out waiting for delete")
            .expect("handler dropped");
        if let WatchEvent::Delete { prev: Some(p), .. } = ev {
            if p.value.pointer("/metadata/name").and_then(Value::as_str) == Some(name) {
                return;
            }
        }
    }
}

fn spawn_informer(
    client: Arc<dyn Client>,
    store: Arc<ObjectStore>,
    handler: EventHandler,
    stop: Stop,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let informer = Informer::new(KeyPrefix::new("", "pods", None));
        informer.run(client, store, handler, stop).await;
    })
}

#[tokio::test]
async fn initial_list_populates_store_and_fires_handler() {
    let store = Arc::new(EmbeddedStorage::new());
    seed(&store, "p1").await;
    seed(&store, "p2").await;
    let client: Arc<dyn Client> = Arc::new(controllers::StorageClient::new(store.clone()));
    let cache = Arc::new(ObjectStore::new());
    let (handler, mut rx) = handlers();
    let stop = Stop::new();
    let h = spawn_informer(client, cache.clone(), handler, stop.clone());
    expect_put(&mut rx, "p1").await;
    expect_put(&mut rx, "p2").await;
    assert_eq!(cache.len(), 2);
    assert!(cache.get("default", "p1").is_some());
    stop.trigger();
    tokio::time::timeout(Duration::from_secs(1), h)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn live_seam_delivers_post_start_writes() {
    let store = Arc::new(EmbeddedStorage::new());
    let client: Arc<dyn Client> = Arc::new(controllers::StorageClient::new(store.clone()));
    let cache = Arc::new(ObjectStore::new());
    let (handler, mut rx) = handlers();
    let stop = Stop::new();
    let h = spawn_informer(client.clone(), cache.clone(), handler, stop.clone());
    // Write through storage directly (the live seam under test).
    store
        .create(&Key::new("", "pods", "default", "late"), pod("late"))
        .await
        .unwrap();
    expect_put(&mut rx, "late").await;
    assert!(cache.get("default", "late").is_some());
    stop.trigger();
    tokio::time::timeout(Duration::from_secs(1), h)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn delete_event_removes_object_from_store() {
    let store = Arc::new(EmbeddedStorage::new());
    seed(&store, "p1").await;
    let client: Arc<dyn Client> = Arc::new(controllers::StorageClient::new(store.clone()));
    let cache = Arc::new(ObjectStore::new());
    let (handler, mut rx) = handlers();
    let stop = Stop::new();
    let h = spawn_informer(client, cache.clone(), handler, stop.clone());
    expect_put(&mut rx, "p1").await;
    store
        .delete(&Key::new("", "pods", "default", "p1"), None)
        .await
        .unwrap();
    expect_delete(&mut rx, "p1").await;
    assert!(cache.get("default", "p1").is_none());
    stop.trigger();
    tokio::time::timeout(Duration::from_secs(1), h)
        .await
        .unwrap()
        .unwrap();
}

/// Client whose `watch` hands back an immediately-closed stream (borrowed
/// from a dropped backend), forcing the informer down its re-list path.
struct ClosedWatchClient {
    store: Arc<EmbeddedStorage>,
    lists: Arc<AtomicUsize>,
}

#[async_trait]
impl Client for ClosedWatchClient {
    async fn list(&self, prefix: &KeyPrefix) -> Result<Vec<Value>, ControllerError> {
        self.lists.fetch_add(1, Ordering::SeqCst);
        Ok(self
            .store
            .list(prefix)
            .await?
            .into_iter()
            .map(|e| e.value)
            .collect())
    }
    async fn get(&self, key: &Key) -> Result<Option<Value>, ControllerError> {
        Ok(self.store.get(key).await?.map(|e| e.value))
    }
    async fn create(&self, key: &Key, value: Value) -> Result<Value, ControllerError> {
        Ok(self.store.create(key, value).await?.value)
    }
    async fn update(
        &self,
        key: &Key,
        value: Value,
        if_revision: Option<u64>,
    ) -> Result<Value, ControllerError> {
        Ok(self.store.update(key, value, if_revision).await?.value)
    }
    async fn delete(&self, key: &Key) -> Result<(), ControllerError> {
        self.store.delete(key, None).await?;
        Ok(())
    }
    async fn watch(
        &self,
        _prefix: &KeyPrefix,
        _start_revision: Option<u64>,
    ) -> Result<storage::Watch, ControllerError> {
        // Subscribe on a short-lived backend, then drop the sender: recv()
        // resolves Closed -> None -> the informer must re-list.
        let tmp = EmbeddedStorage::new();
        let w = tmp.watch(&KeyPrefix::new("", "pods", None), None).await?;
        drop(tmp);
        Ok(w)
    }
}

#[tokio::test]
async fn watch_close_triggers_relist_without_panicking() {
    let store = Arc::new(EmbeddedStorage::new());
    seed(&store, "p1").await;
    let lists = Arc::new(AtomicUsize::new(0));
    let client: Arc<dyn Client> = Arc::new(ClosedWatchClient {
        store: store.clone(),
        lists: lists.clone(),
    });
    let cache = Arc::new(ObjectStore::new());
    let (handler, _rx) = handlers();
    let stop = Stop::new();
    let h = spawn_informer(client, cache.clone(), handler, stop.clone());
    // The watch closes instantly each round; the informer must keep cycling
    // LIST -> (closed) WATCH without panicking and keep the cache accurate.
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while lists.load(Ordering::SeqCst) < 3 {
        assert!(std::time::Instant::now() < deadline, "no re-list happened");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(cache.len(), 1, "cache stays consistent across re-lists");
    stop.trigger();
    tokio::time::timeout(Duration::from_secs(1), h)
        .await
        .unwrap()
        .unwrap();
}
