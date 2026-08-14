//! client-go workqueue semantics (T3.1a).
//!
//! Keys are `namespace/name` strings. The guarantee set: an item is being
//! processed by at most one worker at a time; a re-`add` while processing
//! defers exactly one redelivery on `done`; failures drive exponential
//! backoff via [`backoff_for`]. Workers must drive [`WorkQueue::next`] under
//! `tokio::select!` with a [`crate::Stop`] because the internal receiver is
//! shared through a single mutex and senders are never dropped (the queue
//! outlives any one worker).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::mpsc;

/// Exponential backoff for `failures` consecutive failures: 5ms base,
/// doubling, capped at 1000ms. Pure so tests can pin it.
pub fn backoff_for(failures: u32) -> Duration {
    Duration::from_millis((5u64 << failures.min(8)).min(1000))
}

struct Inner {
    tx: mpsc::UnboundedSender<String>,
    /// Single shared receiver: dequeue is serialized, processing is not.
    receiver: tokio::sync::Mutex<mpsc::UnboundedReceiver<String>>,
    /// Lock order everywhere: `dirty` before `processing`.
    dirty: Mutex<HashSet<String>>,
    processing: Mutex<HashSet<String>>,
    failures: Mutex<HashMap<String, u32>>,
}

/// A deduplicating, rate-limited work queue (client-go `workqueue.Type`).
#[derive(Clone)]
pub struct WorkQueue {
    inner: Arc<Inner>,
}

impl Default for WorkQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl WorkQueue {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        Self {
            inner: Arc::new(Inner {
                tx,
                receiver: tokio::sync::Mutex::new(rx),
                dirty: Mutex::new(HashSet::new()),
                processing: Mutex::new(HashSet::new()),
                failures: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// Enqueue `key`. Already-dirty keys are dropped; keys currently
    /// processing are marked dirty again and re-delivered on `done`.
    pub fn add(&self, key: String) {
        let mut dirty = self.inner.dirty.lock().unwrap();
        if dirty.contains(&key) {
            return;
        }
        if self.inner.processing.lock().unwrap().contains(&key) {
            dirty.insert(key); // deferred; done() re-sends
            return;
        }
        dirty.insert(key.clone());
        let _ = self.inner.tx.send(key);
    }

    /// Dequeue the next key (marks it processing). Returns `None` only when
    /// every sender dropped (never for clones of this queue) -- workers
    /// select on a `Stop` alongside this future.
    pub async fn next(&self) -> Option<String> {
        let key = {
            let mut rx = self.inner.receiver.lock().await;
            rx.recv().await?
        };
        // Same atomicity rule as `done`: keep `dirty` held while the key
        // moves into `processing`, so a concurrent `add` cannot observe a
        // key that is in neither set (a duplicate send is harmless; a lost
        // one is not).
        {
            let mut dirty = self.inner.dirty.lock().unwrap();
            dirty.remove(&key);
            self.inner.processing.lock().unwrap().insert(key.clone());
        }
        Some(key)
    }

    /// Mark a key finished; re-send it if it was re-added while processing.
    pub fn done(&self, key: &str) {
        // Atomic hand-off: hold `dirty` while clearing `processing` (the
        // documented dirty-before-processing order). Releasing `dirty` first
        // would open a window where a concurrent `add` sees the key still
        // processing, defers it into `dirty`, and nobody ever sends it --
        // every later `add` early-returns on `dirty.contains` and the key
        // is permanently undeliverable (the lost-wakeup zombie observed as
        // G18's never-rewritten Deployment status).
        let dirty = self.inner.dirty.lock().unwrap();
        if dirty.contains(key) {
            let _ = self.inner.tx.send(key.to_string());
        }
        self.inner.processing.lock().unwrap().remove(key);
    }

    /// Count (and return) one failure for `key`.
    pub fn note_failure(&self, key: &str) -> u32 {
        let mut f = self.inner.failures.lock().unwrap();
        let n = f.entry(key.to_string()).or_insert(0);
        *n += 1;
        *n
    }

    /// Reset the failure counter for `key` (on success).
    pub fn forget(&self, key: &str) {
        self.inner.failures.lock().unwrap().remove(key);
    }

    /// Add `key` after `delay` (fire-and-forget task).
    pub fn add_after(&self, key: String, delay: Duration) {
        let q = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            q.add(key);
        });
    }

    /// Rate-limited requeue: bumps the failure counter and re-adds after
    /// [`backoff_for`] the observed failure count.
    pub fn add_rate_limited(&self, key: String) {
        let n = self.note_failure(&key);
        self.add_after(key, backoff_for(n));
    }

    /// Number of pending (dirty) keys; for tests and introspection.
    pub fn len(&self) -> usize {
        self.inner.dirty.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_dedupes() {
        let q = WorkQueue::new();
        q.add("ns/a".into());
        q.add("ns/a".into());
        assert_eq!(q.len(), 1);
    }

    #[tokio::test]
    async fn next_clears_dirty_and_marks_processing() {
        let q = WorkQueue::new();
        q.add("ns/a".into());
        assert_eq!(q.next().await.as_deref(), Some("ns/a"));
        assert_eq!(q.len(), 0);
        q.done("ns/a");
    }

    /// Regression (T3.1b golden G18 hang): a concurrent `add` landing
    /// between `done`'s dirty check and its `processing` removal used to
    /// defer the key into `dirty` with no sender, making it permanently
    /// undeliverable. Hammer `add` from threads while next/done churn to
    /// prove the hand-off is lossless: every pending add must be delivered.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_add_during_done_never_loses_keys() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let q = WorkQueue::new();
        let alive = Arc::new(AtomicBool::new(true));
        let adder = {
            let q = q.clone();
            let alive = alive.clone();
            std::thread::spawn(move || {
                while alive.load(Ordering::Relaxed) {
                    q.add("ns/z".into());
                }
            })
        };
        // next/done churn against the concurrent adder: the queue is
        // non-empty the whole time, so each iteration is fast.
        for _ in 0..2_000 {
            let got = tokio::time::timeout(Duration::from_millis(100), q.next()).await;
            if let Ok(Some(k)) = got {
                assert_eq!(k, "ns/z");
                q.done("ns/z");
            }
        }
        alive.store(false, Ordering::Relaxed);
        adder.join().unwrap();
        // After the adder stops, one final add must still be delivered --
        // with the pre-fix lost hand-off the key stayed stuck in `dirty`.
        q.add("ns/z".into());
        let got = tokio::time::timeout(Duration::from_millis(500), q.next()).await;
        assert_eq!(
            got.expect("final add must be delivered").as_deref(),
            Some("ns/z")
        );
        q.done("ns/z");
        assert_eq!(q.len(), 0);
    }

    #[tokio::test]
    async fn readd_while_processing_defers_until_done() {
        let q = WorkQueue::new();
        q.add("ns/a".into());
        assert_eq!(q.next().await.as_deref(), Some("ns/a"));
        q.add("ns/a".into()); // deferred: dirty, not queued
        assert_eq!(q.len(), 1);
        // Nothing deliverable while the key is processing.
        assert!(tokio::time::timeout(Duration::from_millis(30), q.next())
            .await
            .is_err());
        q.done("ns/a");
        assert_eq!(q.next().await.as_deref(), Some("ns/a"));
        q.done("ns/a");
    }

    #[test]
    fn backoff_is_monotonic_up_to_cap() {
        assert_eq!(backoff_for(0), Duration::from_millis(5));
        let mut prev = Duration::ZERO;
        for n in 0..12u32 {
            let d = backoff_for(n);
            assert!(d >= prev, "backoff must be monotonic at {n}");
            assert!(d <= Duration::from_millis(1000));
            prev = d;
        }
        assert_eq!(backoff_for(8), Duration::from_millis(1000));
        assert_eq!(backoff_for(50), Duration::from_millis(1000));
    }

    #[test]
    fn forget_resets_failures() {
        let q = WorkQueue::new();
        assert_eq!(q.note_failure("ns/a"), 1);
        assert_eq!(q.note_failure("ns/a"), 2);
        q.forget("ns/a");
        assert_eq!(q.note_failure("ns/a"), 1);
    }

    #[tokio::test]
    async fn add_after_delivers_after_delay() {
        let q = WorkQueue::new();
        q.add_after("ns/k".into(), Duration::from_millis(5));
        let got = tokio::time::timeout(Duration::from_millis(500), q.next())
            .await
            .expect("timed out")
            .expect("channel closed");
        assert_eq!(got, "ns/k");
    }

    #[tokio::test]
    async fn add_rate_limited_uses_backoff() {
        let q = WorkQueue::new();
        q.note_failure("ns/k");
        q.note_failure("ns/k"); // 2 failures -> 20ms
        q.add_rate_limited("ns/k".into());
        let got = tokio::time::timeout(Duration::from_millis(500), q.next())
            .await
            .expect("timed out")
            .expect("channel closed");
        assert_eq!(got, "ns/k");
    }
}
