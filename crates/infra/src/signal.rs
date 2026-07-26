//! Graceful shutdown coordination (TODO **T0.3**).
//!
//! A [`Shutdown`] is a cheaply-clonable token. [`install`] spawns a task that
//! triggers it on SIGTERM or SIGINT (k3s uses the same pair), so every
//! long-lived component can `.await` [`Shutdown::cancelled`] and drain.

use std::sync::Arc;
use tokio::sync::Notify;

/// Cancellation token shared across components.
#[derive(Clone, Debug)]
pub struct Shutdown {
    inner: Arc<Notify>,
}

impl Shutdown {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Notify::new()),
        }
    }

    /// Fire the token; all waiters wake.
    pub fn trigger(&self) {
        self.inner.notify_waiters()
    }

    /// Resolve when the token is fired (or never, if it never is).
    pub async fn cancelled(&self) {
        self.inner.notified().await;
    }
}

impl Default for Shutdown {
    fn default() -> Self {
        Self::new()
    }
}

/// Install signal handlers that fire `shutdown` on SIGTERM/SIGINT.
///
/// Safe to call once per process. On non-unix targets this is a no-op success.
#[cfg(unix)]
pub async fn install(shutdown: Shutdown) -> std::io::Result<()> {
    use tokio::signal::unix::{signal, SignalKind};

    let mut term = signal(SignalKind::terminate())?;
    let mut int = signal(SignalKind::interrupt())?;

    tokio::spawn(async move {
        tokio::select! {
            _ = term.recv() => {
                tracing::info!(target: "init-pro", "received SIGTERM — shutting down");
            }
            _ = int.recv() => {
                tracing::info!(target: "init-pro", "received SIGINT — shutting down");
            }
        }
        shutdown.trigger();
    });
    Ok(())
}

#[cfg(not(unix))]
pub async fn install(_shutdown: Shutdown) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn trigger_wakes_cancelled() {
        let s = Shutdown::new();
        let s2 = s.clone();
        // Fire from a task; the waiter must resolve.
        tokio::spawn(async move {
            tokio::task::yield_now().await;
            s2.trigger();
        });
        // Must not hang.
        let r = tokio::time::timeout(std::time::Duration::from_secs(1), s.cancelled()).await;
        assert!(r.is_ok(), "cancelled() must resolve after trigger()");
    }

    #[tokio::test]
    async fn untriggered_never_resolves() {
        let s = Shutdown::new();
        let r = tokio::time::timeout(std::time::Duration::from_millis(10), s.cancelled()).await;
        assert!(r.is_err(), "cancelled() must not resolve without trigger()");
    }

    #[test]
    fn install_returns_ok() {
        // install spawns a background task; we only assert it does not error.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let s = Shutdown::new();
        let res = rt.block_on(install(s));
        assert!(res.is_ok(), "install must succeed: {res:?}");
    }
}
