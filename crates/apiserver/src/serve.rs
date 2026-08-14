//! Server entry point: bind, route discovery + REST, drain on shutdown (T1.2).
//!
//! [`serve`] is process-agnostic: it binds a `TcpListener`, mounts the
//! [`crate::app::api_app`] router (discovery + CRUD + watch), and runs until
//! the provided shutdown future resolves, at which point axum drains in-flight
//! connections gracefully (mirrors the `graceful-shutdown-test.sh` contract).
//!
//! The shutdown signal is a generic future (not the `infra::Shutdown` type) so
//! this crate stays decoupled from the infra layer: the caller decides what
//! cancels the server.

use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;

use api::SchemaRegistry;
use axum::Router;
use storage::StorageBackend;

use crate::app::router;
use crate::state::AppState;

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

    // axum::serve stops accepting as soon as the graceful-shutdown future
    // resolves, then drains in-flight requests before returning.
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await?;

    tracing::info!(target: "init-pro", "apiserver drained");
    Ok(())
}
