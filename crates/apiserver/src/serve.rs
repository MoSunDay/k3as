//! Server entry point: bind, route discovery + REST, drain on shutdown (T1.2).
//!
//! [`serve`] is process-agnostic: it binds a `TcpListener`, mounts the
//! [`crate::app::api_app`] router (discovery + CRUD + watch), and runs until
//! the provided shutdown future resolves, at which point axum drains in-flight
//! connections gracefully — but only up to a bounded deadline, since infinite
//! watch streams would otherwise hold the process open forever (mirrors the
//! `graceful-shutdown-test.sh` contract).
//!
//! The shutdown signal is a generic future (not the `infra::Shutdown` type) so
//! this crate stays decoupled from the infra layer: the caller decides what
//! cancels the server.

use std::future::{Future, IntoFuture};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use api::SchemaRegistry;
use axum::Router;
use storage::StorageBackend;

use crate::app::router;
use crate::state::AppState;

/// Upper bound on the graceful drain once shutdown fires: watch streams
/// (kubelet informers, T4.2) never complete on their own, so without a
/// deadline one connected watcher holds the process open forever. Fits the
/// 5s SIGTERM-exit contract of scripts/graceful-shutdown-test.sh.
const DRAIN_GRACE: Duration = Duration::from_secs(2);

/// Run the API server until `shutdown` resolves.
///
/// `server_address` is the host:port advertised in the `/api` `APIVersions`
/// document (k3s surfaces the bind address here).
pub async fn serve<F>(
    registry: SchemaRegistry,
    store: Arc<dyn StorageBackend>,
    addr: SocketAddr,
    server_address: String,
    shutdown: F,
) -> std::io::Result<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(target: "init-pro", addr = %addr, "apiserver discovery listening (HTTP)");

    let app: Router = router(AppState {
        registry: Arc::new(registry),
        store,
        server_address,
    });

    let (fired_tx, fired_rx) = tokio::sync::oneshot::channel::<()>();
    let shutdown = async move {
        shutdown.await;
        let _ = fired_tx.send(());
    };

    // axum::serve stops accepting as soon as the graceful-shutdown future
    // resolves, then drains in-flight requests before returning.
    // `.into_future()` pins the concrete serve future (axum's
    // `WithGracefulShutdown` only implements `IntoFuture`) so it can be
    // raced against the drain deadline below.
    let server = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .into_future();
    // Drain deadline: fires DRAIN_GRACE after the shutdown signal.
    let deadline = async move {
        if fired_rx.await.is_ok() {
            tokio::time::sleep(DRAIN_GRACE).await;
        }
    };
    tokio::pin!(server);
    tokio::pin!(deadline);
    tokio::select! {
        res = &mut server => res?,
        () = &mut deadline => {
            tracing::warn!(
                target: "init-pro",
                grace_secs = DRAIN_GRACE.as_secs(),
                "apiserver drain deadline reached; dropping lingering connections (open watch streams)"
            );
        }
    }

    tracing::info!(target: "init-pro", "apiserver drained");
    Ok(())
}
