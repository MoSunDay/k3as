//! The Ingress gateway (workstream A, decision **Q32**).
//!
//! One openrusty runtime serving `networking.k8s.io/v1` Ingress traffic on
//! `ingress_listen`: a watcher recomputes the compiled [`RouteTable`] from
//! the folded Ingress set ([`crate::ingress_watch`]) plus live endpoints
//! ([`crate::endpoints`]) and hot-applies a regenerated openrusty config;
//! the serve task fronts that runtime with the [`crate::gate`] (the Q28
//! 503-while-empty parity, host/path aware via the published table).
//!
//! Lifecycle: the gateway runs on the same dedicated plane thread as the
//! NodePort listeners ([`crate::nodeport`]) and drains with it. The bound
//! address (tests may pass `:0`) is published through the plane's
//! `OnceLock`.

use std::net::SocketAddr;
use std::sync::Arc;

use infra::Shutdown;
use openrusty_server::state::{self as or_state, AppState};
use tokio::sync::watch;

use crate::endpoints::{resolve_with_state, EndpointsResolver};
use crate::gate::{GateRoute, GatedRouter};
use crate::gateway_config;
use crate::ingress::compile_ingress;
use crate::route::RouteTable;
use crate::serve;

/// Config-path marker recorded in the openrusty runtime (no file reloads).
const CONFIG_PATH: &str = "init-pro://router/ingress";

/// Run the gateway until `shutdown` fires; publishes the bound address to
/// `bound` once listening.
pub(crate) async fn run(
    listen: SocketAddr,
    resolver: EndpointsResolver,
    shutdown: Shutdown,
    bound: Arc<std::sync::OnceLock<SocketAddr>>,
) {
    let listener = match serve::bind(listen).await {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!(target: "init-pro", error = %e, "ingress gateway bind failed; gateway disabled");
            return;
        }
    };
    let addr = listener.local_addr().unwrap_or(listen);
    if bound.set(addr).is_err() {
        tracing::debug!(target: "init-pro", "ingress gateway address already published");
    }

    // Initial compile + bootstrap, then serve; recompiles ride the loop on
    // BOTH channels: endpoint folds (peer sets) and Ingress folds (routes).
    let mut state_rx = resolver.subscribe();
    let mut ingress_rx = resolver.subscribe_ingresses();
    let snapshot = state_rx.borrow_and_update().clone();
    let table = compile(&resolver);
    let (table_tx, table_rx) = watch::channel(Arc::new(table.clone()));
    let cfg = gateway_config::ingress_config(addr, &table, &|up: &crate::route::UpstreamRef| {
        resolve_with_state(&snapshot, up)
    });
    let app: Arc<AppState> = match or_state::from_config(cfg, std::path::PathBuf::from(CONFIG_PATH))
    {
        Ok(app) => app,
        Err(e) => {
            tracing::warn!(target: "init-pro", error = %e, "ingress runtime bootstrap failed; gateway disabled");
            return;
        }
    };
    let gated = GatedRouter::new(
        openrusty_server::app::router(app.clone()),
        GateRoute::Table(Arc::new(table_rx)),
        resolver.clone(),
    );
    tokio::spawn(serve::serve_until(listener, gated, shutdown.clone()));
    tracing::info!(target: "init-pro", %addr, rules = table.len(), "ingress gateway up");

    // Recompile + hot-apply on every fold (Ingress or endpoint changes).
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => {
                tracing::info!(target: "init-pro", %addr, "ingress gateway draining");
                return;
            }
            changed = state_rx.changed() => {
                if changed.is_err() {
                    return; // resolver channels dropped (plane teardown)
                }
            }
            changed = ingress_rx.changed() => {
                if changed.is_err() {
                    return; // resolver channels dropped (plane teardown)
                }
            }
        }
        let snapshot = state_rx.borrow_and_update().clone();
        let table = compile(&resolver);
        let cfg =
            gateway_config::ingress_config(addr, &table, &|up: &crate::route::UpstreamRef| {
                resolve_with_state(&snapshot, up)
            });
        match openrusty_server::reload::apply_config(&app, &cfg).await {
            Ok(generation) => {
                let _ = table_tx.send(Arc::new(table));
                tracing::debug!(
                    target: "init-pro",
                    generation,
                    "ingress config hot-applied"
                );
            }
            Err(e) => {
                // Keep serving the last good generation (fail-static).
                tracing::warn!(target: "init-pro", error = %e, "ingress config recompile failed; keeping last good generation")
            }
        }
    }
}

/// Compile the expressible route table from the folded Ingress set.
fn compile(resolver: &EndpointsResolver) -> RouteTable {
    let ingresses: Vec<api::networking::Ingress> =
        resolver.ingress_table().values().cloned().collect();
    let compiled = compile_ingress(&ingresses);
    gateway_config::expressible(&compiled)
}
