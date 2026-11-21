//! Server entry point: bind, route discovery + REST, drain on shutdown (T1.2).
//!
//! [`serve`] is process-agnostic: it binds a `TcpListener`, mounts the
//! [`crate::app::api_app`] router (discovery + CRUD + watch), and runs until
//! the provided shutdown future resolves, at which point axum drains in-flight
//! connections gracefully — but only up to a bounded deadline, since infinite
//! watch streams would otherwise hold the process open forever (mirrors the
//! `graceful-shutdown-test.sh` contract).
//!
//! T1.3a (Q47) adds two opt-in extras via [`ServeOptions`]: static-token
//! authn (+ RBAC authz bootstrap) and serving TLS. Both default to `None`
//! and the default posture is byte-identical to the pre-T1.3 h2c plane.
//!
//! The shutdown signal is a generic future (not the `infra::Shutdown` type) so
//! this crate stays decoupled from the infra layer: the caller decides what
//! cancels the server.

use std::future::{Future, IntoFuture};
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use api::SchemaRegistry;
use axum::Router;

use storage::StorageBackend;

use crate::app::router;
use crate::authn::TokenAuth;
use crate::state::AppState;
use crate::tls::TlsAcceptor;

/// Drain grace after the shutdown signal: ordinary requests finish; open
/// watch streams (which never end on their own) are dropped at the deadline.
const DRAIN_GRACE: Duration = Duration::from_secs(2);

/// Optional serving extras threaded from the CLI plane (T1.3a, Q47).
#[derive(Default)]
pub struct ServeOptions {
    /// The anyhome NFS endpoint behind per-Pod anyhome defaulting
    /// (T7.5, Q43); `None` = annotations inert.
    pub workspace_nfs: Option<String>,
    /// Static-token authentication. `Some` also layers the RBAC authz
    /// middleware and (once, before bind) bootstraps the default
    /// `cluster-admin` + `system:nodes` bindings.
    pub auth: Option<Arc<TokenAuth>>,
    /// Serving TLS. `Some` switches the bound listener to a TLS listener
    /// (self-signed CA chain minted upstream in the CLI plane).
    pub tls: Option<TlsAcceptor>,
}

/// Run the API server until `shutdown` resolves.
///
/// `server_address` is the host:port advertised in the `/api` `APIVersions`
/// document (k3s surfaces the bind address here).
pub async fn serve<F>(
    registry: SchemaRegistry,
    store: Arc<dyn StorageBackend>,
    addr: SocketAddr,
    server_address: String,
    options: ServeOptions,
    shutdown: F,
) -> std::io::Result<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    let ServeOptions {
        workspace_nfs,
        auth,
        tls,
    } = options;

    // Auth on -> RBAC on: seed the default bindings once (idempotent).
    if auth.is_some() {
        crate::authz_bootstrap::bootstrap_rbac(&store).await;
    }

    let listener = tokio::net::TcpListener::bind(addr).await?;
    // Keep the historical "discovery listening" substring: the e2e/golden
    // scripts gate server readiness on it (T1.3a adds the tls/auth flags).
    tracing::info!(target: "init-pro", addr = %addr, tls = tls.is_some(), auth = auth.is_some(), "apiserver discovery listening");

    let app: Router = router(AppState {
        registry: Arc::new(registry),
        store,
        server_address,
        workspace_nfs,
        auth,
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
    // raced against the drain deadline below; the two listener shapes
    // (plain TCP vs TLS adapter) unify behind one boxed future.
    let server: Pin<Box<dyn Future<Output = std::io::Result<()>> + Send>> = match tls {
        Some(acceptor) => {
            let tls_listener = crate::tls::TlsListener::new(listener, acceptor);
            Box::pin(
                axum::serve(tls_listener, app)
                    .with_graceful_shutdown(shutdown)
                    .into_future(),
            )
        }
        None => Box::pin(
            axum::serve(listener, app)
                .with_graceful_shutdown(shutdown)
                .into_future(),
        ),
    };
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
