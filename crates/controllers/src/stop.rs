//! Self-contained, LATCHED stop token (T3.1a).
//!
//! Mirrors `infra::Shutdown`'s role (cheaply clonable, wakes all waiters) but
//! is duplicated deliberately so the controllers crate stays decoupled from
//! `infra` (decision **Q19**: controllers talk to storage in-process and
//! carry no server plumbing). Unlike a bare `Notify`, the fired state is
//! LATCHED: `trigger()` before a `cancelled()` poll still resolves it, so a
//! task that is between two `select!` phases cannot miss the edge.

use std::sync::Arc;

use tokio::sync::watch;

/// Cancellation token shared across controller tasks.
#[derive(Clone, Debug)]
pub struct Stop {
    tx: Arc<watch::Sender<bool>>,
}

impl Stop {
    pub fn new() -> Self {
        let (tx, _rx) = watch::channel(false);
        Self { tx: Arc::new(tx) }
    }

    /// Fire the token (idempotent); all current and future waiters resolve.
    pub fn trigger(&self) {
        self.tx.send_replace(true);
    }

    /// True once [`Stop::trigger`] has fired.
    pub fn is_triggered(&self) -> bool {
        *self.tx.borrow()
    }

    /// Resolve when (or if already) the token has been fired.
    pub async fn cancelled(&self) {
        if self.is_triggered() {
            return;
        }
        let mut rx = self.tx.subscribe();
        let _ = rx.wait_for(|v| *v).await;
    }
}

impl Default for Stop {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn trigger_before_await_still_resolves() {
        let stop = Stop::new();
        stop.trigger();
        stop.cancelled().await; // must not hang
        assert!(stop.is_triggered());
    }

    #[tokio::test]
    async fn trigger_wakes_parked_waiter() {
        let stop = Stop::new();
        let s2 = stop.clone();
        let task = tokio::spawn(async move {
            s2.cancelled().await;
        });
        tokio::time::sleep(Duration::from_millis(10)).await;
        stop.trigger();
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("waiter woke")
            .expect("task ok");
    }

    #[tokio::test]
    async fn untriggered_stays_pending() {
        let stop = Stop::new();
        let pending = tokio::time::timeout(Duration::from_millis(30), stop.cancelled()).await;
        assert!(pending.is_err(), "must not resolve without trigger");
    }
}
