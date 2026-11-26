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
//!
//! Change detection (T5.4 hot reload, **B6**): both watch channels fold on
//! EVERY reconcile, so an unrelated Endpoints/Ingress write used to
//! recompile and re-apply an identical config. Each fold now hashes the
//! inputs of the generated config ([`fingerprint`]) and compares it with
//! the generation already serving ([`ApplyLedger`]); an unchanged
//! fingerprint skips the compile + apply + reload entirely (debug log).
//! When the routes DO change the path is exactly the pre-B6 one: compile,
//! [`gateway_config::ingress_config`], `apply_config`, publish the table.

use std::collections::hash_map::DefaultHasher;
use std::fmt::Write as _;
use std::hash::{Hash, Hasher};
use std::net::SocketAddr;
use std::sync::Arc;

use infra::Shutdown;
use openrusty_server::state::{self as or_state, AppState};
use tokio::sync::watch;

use crate::endpoints::{resolve_with_state, EndpointsResolver};
use crate::gate::{GateRoute, GatedRouter};
use crate::gateway_config;
use crate::ingress::compile_ingress;
use crate::route::{HostMatcher, PathMatcher, PortRef, RouteTable, UpstreamRef};
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
    let cfg = gateway_config::ingress_config(addr, &table, &|up: &UpstreamRef| {
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
    // The bootstrapped generation is the baseline every later fold is
    // compared against (B6): the first no-op reconcile must not re-apply.
    let mut ledger = ApplyLedger::new();
    ledger.record(fingerprint(addr, &table, &|up| {
        resolve_with_state(&snapshot, up)
    }));
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
        // B6: hash the compiled product first; `fold_once` then builds the
        // openrusty config and reloads the runtime ONLY when it changed
        // (the table compile above is the cheap pure part).
        let fingerprint = fingerprint(addr, &table, &|up| resolve_with_state(&snapshot, up));
        let cfg_table = table.clone();
        let cfg_app = app.clone();
        let outcome = fold_once(&mut ledger, fingerprint, move || {
            let cfg = gateway_config::ingress_config(addr, &cfg_table, &|up: &UpstreamRef| {
                resolve_with_state(&snapshot, up)
            });
            async move {
                openrusty_server::reload::apply_config(&cfg_app, &cfg)
                    .await
                    .map_err(|e| e.to_string())
            }
        })
        .await;
        match outcome {
            FoldOutcome::Applied(generation) => {
                let _ = table_tx.send(Arc::new(table));
                tracing::debug!(
                    target: "init-pro",
                    generation,
                    "ingress config hot-applied"
                );
            }
            // Nothing changed: the serving generation already matches.
            FoldOutcome::Unchanged => {}
            FoldOutcome::Failed(e) => {
                // Keep serving the last good generation (fail-static).
                tracing::warn!(target: "init-pro", error = %e, "ingress config recompile failed; keeping last good generation")
            }
        }
    }
}

/// Deterministic hash of one compiled generation (B6): everything the
/// generated openrusty config depends on, and nothing else.
///
/// Canonical form (in order): the listener, every rule in finalised order
/// (host matcher, path matcher, upstream) with the peers it resolves to
/// RIGHT NOW, then the default backend and its peers. [`RouteTable`]'s
/// `generation` stamp and each rule's `origin` label are deliberately
/// excluded - neither reaches the data plane, so a fold that only bumps
/// them is a genuine no-op.
///
/// `DefaultHasher` (SipHash-1-3 with fixed keys) is deterministic for a
/// given std version, which is all this needs: the hash lives only inside
/// this process, never on disk or on the wire.
fn fingerprint(
    listen: SocketAddr,
    table: &RouteTable,
    peers_of: &dyn Fn(&UpstreamRef) -> Vec<SocketAddr>,
) -> Fingerprint {
    let mut canon = String::with_capacity(128 + 64 * table.len());
    let _ = write!(canon, "listen={listen};");
    for rule in table.rules() {
        let _ = write!(
            canon,
            "rule host={} path={} upstream={}",
            host_key(&rule.host),
            path_key(&rule.path),
            upstream_key(&rule.upstream)
        );
        write_peers(&mut canon, &peers_of(&rule.upstream));
    }
    canon.push_str(";default=");
    match table.default_upstream() {
        Some(up) => {
            let _ = write!(canon, "{}", upstream_key(up));
            write_peers(&mut canon, &peers_of(up));
        }
        None => canon.push('-'),
    }
    let mut hasher = DefaultHasher::new();
    canon.hash(&mut hasher);
    hasher.finish()
}

/// Fingerprint of one compiled generation (see [`fingerprint`]).
type Fingerprint = u64;

fn host_key(host: &HostMatcher) -> String {
    match host {
        HostMatcher::Any => "any".to_string(),
        HostMatcher::Exact(h) => format!("exact:{h}"),
        HostMatcher::Wildcard(s) => format!("wildcard:{s}"),
    }
}

fn path_key(path: &PathMatcher) -> String {
    match path {
        PathMatcher::Exact(p) => format!("exact:{p}"),
        PathMatcher::Prefix(p) => format!("prefix:{p}"),
    }
}

fn upstream_key(up: &UpstreamRef) -> String {
    match &up.port {
        PortRef::Number(p) => format!("{}:{}", up.service, p),
        PortRef::Named(p) => format!("{}:{}", up.service, p),
    }
}

/// Peer list in resolver order (a reorder changes the emitted upstream
/// entry, so it is part of the identity, not noise).
fn write_peers(canon: &mut String, peers: &[SocketAddr]) {
    canon.push('[');
    for peer in peers {
        let _ = write!(canon, "{peer},");
    }
    canon.push(']');
}

/// Which generation is currently serving (B6). Held in the gateway's own
/// state - no process globals - and only advanced on a SUCCESSFUL apply,
/// so a failed reload keeps the last good generation and retries on the
/// next fold instead of being swallowed as "unchanged".
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct ApplyLedger {
    last: Option<Fingerprint>,
}

impl ApplyLedger {
    /// Empty ledger: nothing is serving yet, so the first fold applies.
    fn new() -> Self {
        Self { last: None }
    }

    /// True when `next` is not what is already serving.
    fn needs_apply(&self, next: Fingerprint) -> bool {
        self.last != Some(next)
    }

    /// Record `next` as the serving generation.
    fn record(&mut self, next: Fingerprint) {
        self.last = Some(next);
    }
}

/// What one fold did.
#[derive(Debug, Clone, PartialEq, Eq)]
enum FoldOutcome {
    /// Compiled product identical to the serving generation: no apply.
    Unchanged,
    /// A new generation was hot-applied (openrusty generation id).
    Applied(u64),
    /// The apply failed; the last good generation keeps serving.
    Failed(String),
}

/// One fold of the apply loop (B6): skip the compile/apply/reload entirely
/// when `fingerprint` matches the serving generation, otherwise run
/// `apply` and record its outcome. `apply` is injected so the skip is
/// unit-testable without a gateway runtime or a socket.
async fn fold_once<Fut>(
    ledger: &mut ApplyLedger,
    fingerprint: Fingerprint,
    apply: impl FnOnce() -> Fut,
) -> FoldOutcome
where
    Fut: std::future::Future<Output = Result<u64, String>>,
{
    if !ledger.needs_apply(fingerprint) {
        tracing::debug!(
            target: "init-pro",
            fingerprint,
            "ingress config unchanged; recompile + reload skipped"
        );
        return FoldOutcome::Unchanged;
    }
    match apply().await {
        Ok(generation) => {
            ledger.record(fingerprint);
            FoldOutcome::Applied(generation)
        }
        Err(e) => FoldOutcome::Failed(e),
    }
}

/// Compile the expressible route table from the folded Ingress set.
fn compile(resolver: &EndpointsResolver) -> RouteTable {
    let ingresses: Vec<api::networking::Ingress> =
        resolver.ingress_table().values().cloned().collect();
    let compiled = compile_ingress(&ingresses);
    gateway_config::expressible(&compiled)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(port: u16) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], port))
    }

    /// One exact-host rule plus an optional default backend (the shapes
    /// the Ingress compiler emits); `origin` is caller-controlled so the
    /// fingerprint test can prove it is ignored.
    fn sample(origin: &str, with_default: bool) -> RouteTable {
        let mut table = RouteTable::new();
        table.push(crate::route::RouteRule {
            host: HostMatcher::new("api.example.com"),
            path: PathMatcher::new("Prefix", "/v1"),
            upstream: UpstreamRef::port("api", 80),
            origin: origin.to_string(),
        });
        table.push(crate::route::RouteRule {
            host: HostMatcher::Any,
            path: PathMatcher::new("Exact", "/exact"),
            upstream: UpstreamRef {
                service: "web".to_string(),
                port: PortRef::Named("http".to_string()),
            },
            origin: origin.to_string(),
        });
        if with_default {
            table = table.with_default(UpstreamRef::port("edge", 8080));
        }
        table.finalise()
    }

    fn sample_table() -> RouteTable {
        sample("ingress/default/edge", true)
    }

    fn fp(listen: SocketAddr, table: &RouteTable, peers: &[SocketAddr]) -> Fingerprint {
        fingerprint(listen, table, &|_| peers.to_vec())
    }

    #[test]
    fn fingerprint_is_deterministic_and_tracks_only_real_changes() {
        let table = sample_table();
        let peer = addr(8081);
        let base = fp(addr(8080), &table, &[peer]);

        // Deterministic: the same inputs hash identically on every fold.
        assert_eq!(base, fp(addr(8080), &table, &[peer]));

        // Endpoint churn is a real change (the emitted upstream differs).
        assert_ne!(base, fp(addr(8080), &table, &[addr(9091)]));
        assert_ne!(base, fp(addr(8080), &table, &[peer, addr(9091)]));
        // So is the listener and a peer reorder.
        assert_ne!(base, fp(addr(8081), &table, &[peer]));
        assert_ne!(
            fp(addr(8080), &table, &[addr(9091), peer]),
            fp(addr(8080), &table, &[peer, addr(9091)])
        );

        // The `origin` label is tracing-only: relabelling is NOT a change.
        let relabelled = sample("ingress/other/edge", true);
        assert_eq!(base, fp(addr(8080), &relabelled, &[peer]));

        // Dropping the default backend is a change.
        let no_default = sample("ingress/default/edge", false);
        assert_ne!(base, fp(addr(8080), &no_default, &[peer]));
    }

    /// Counting `apply` for the fold tests: one increment per real apply.
    async fn fold(
        ledger: &mut ApplyLedger,
        next: Fingerprint,
        applies: &std::rc::Rc<std::cell::Cell<u32>>,
    ) -> FoldOutcome {
        let applies = std::rc::Rc::clone(applies);
        fold_once(ledger, next, move || {
            let applies = std::rc::Rc::clone(&applies);
            async move {
                applies.set(applies.get() + 1);
                Ok(u64::from(applies.get()))
            }
        })
        .await
    }

    #[tokio::test]
    async fn unchanged_folds_skip_the_apply_and_changed_folds_apply() {
        let table = sample_table();
        let same = fp(addr(8080), &table, &[addr(8081)]);
        let moved = fp(addr(8080), &table, &[addr(9091)]);
        let applies = std::rc::Rc::new(std::cell::Cell::new(0u32));
        let mut ledger = ApplyLedger::new();

        // First fold of a fresh gateway always applies (bootstrap).
        assert_eq!(
            fold(&mut ledger, same, &applies).await,
            FoldOutcome::Applied(1)
        );
        // A reconcile that changes nothing must NOT apply again.
        assert_eq!(
            fold(&mut ledger, same, &applies).await,
            FoldOutcome::Unchanged
        );
        assert_eq!(
            applies.get(),
            1,
            "an identical fold recompiled + reloaded the gateway"
        );
        // Endpoints moved -> the compiled product changed -> apply.
        assert_eq!(
            fold(&mut ledger, moved, &applies).await,
            FoldOutcome::Applied(2)
        );
        assert_eq!(applies.get(), 2);
        // ... and the new generation is now the baseline.
        assert_eq!(
            fold(&mut ledger, moved, &applies).await,
            FoldOutcome::Unchanged
        );
        assert_eq!(applies.get(), 2);
    }

    #[tokio::test]
    async fn a_failed_apply_is_not_recorded_as_serving() {
        let mut ledger = ApplyLedger::new();
        let outcome = fold_once(&mut ledger, 42, || async {
            Err("reload blew up".to_string())
        })
        .await;
        assert_eq!(outcome, FoldOutcome::Failed("reload blew up".into()));
        // The next fold must retry instead of treating 42 as already live.
        assert!(ledger.needs_apply(42));
        assert_eq!(
            fold(&mut ledger, 42, &std::rc::Rc::new(std::cell::Cell::new(0))).await,
            FoldOutcome::Applied(1)
        );
        assert!(!ledger.needs_apply(42));
    }
}
