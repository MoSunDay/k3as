//! Server entry point: bind, route discovery, drain on shutdown (TODO **T1.2a**).
//!
//! [`serve`] is process-agnostic: it binds a `TcpListener`, mounts the
//! [`discovery_app`] router, and runs until the provided shutdown future
//! resolves, at which point axum drains in-flight connections gracefully
//! (mirrors the `graceful-shutdown-test.sh` contract).
//!
//! The shutdown signal is a generic future (not the `infra::Shutdown`
//! type) so this crate stays decoupled from the infra layer: the caller decides
//! what cancels the server. The caller (in `cli::runtime`) spawns this
//! future and joins it after its own shutdown resolves, so the process does not
//! exit until the server has fully drained.

use std::future::Future;
use std::net::SocketAddr;

use axum::Router;
use api::SchemaRegistry;

use crate::discovery_handlers::discovery_app;

/// Run the discovery API server until `shutdown` resolves.
///
/// `server_address` is the host:port advertised in the `/api` `APIVersions`
/// document (k3s surfaces the bind address here).
pub async fn serve<F>(
    registry: SchemaRegistry,
    addr: SocketAddr,
    server_address: String,
    shutdown: F,
) -> std::io::Result<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(target: "init-pro", addr = %addr, "apiserver discovery listening (HTTP)");

    let app: Router = discovery_app(registry, server_address);

    // axum::serve stops accepting as soon as the graceful-shutdown future
    // resolves, then drains in-flight requests before returning.
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await?;

    tracing::info!(target: "init-pro", "apiserver drained");
    Ok(())
}
