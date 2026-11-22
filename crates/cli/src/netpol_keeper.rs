//! Node-side NetworkPolicy enforcement keeper (TODO **T4.4**, decision
//! **Q64**): LIST/watch-scan loop that recompiles the iptables program
//! every [`INTERVAL`] and applies it atomically.
//!
//! Full-regen polling (not incremental watch) is deliberate: the rule
//! program is a pure function of (pods, policies, namespaces) — a
//! 5s resync makes convergence idempotent and self-healing (deleted
//! pods prune their chains, deleted policies drop isolation) at e2e
//! scale, with no reflected-state bookkeeping to drift.
//!
//! Lists go through the apiserver cluster-path collections
//! (`/api/v1/pods`, `/api/v1/namespaces`,
//! `/apis/networking.k8s.io/v1/networkpolicies` — Q10 JSON-only).
//! Any fetch failure skips the tick with a WARN (enforcement pauses at
//! the last good program; the agent never goes down). On shutdown the
//! keeper tears the whole chain namespace down — zero residue (NP4) —
//! and the drain handle lets `run_supervised` wait for that teardown
//! before the process exits.

use std::sync::Arc;
use std::time::Duration;

use infra::Shutdown;
use kubelet::HttpJson;
use serde_json::Value;

use crate::netpol_compile::compile_netpol;
use crate::netpol_iptables::{apply_program, ensure_dispatch_jump, teardown};

/// Resync interval (k3s/kube-proxy-class order of magnitude).
const INTERVAL: Duration = Duration::from_secs(5);

/// Spawn the keeper; the returned handle is drained by `run_supervised`
/// after flanneld (teardown must complete before process exit, NP4).
/// Spawned ONLY on non-builtin nodes with flannel enabled (Q52: builtin
/// pods share the host netns; the gate lives in `runtime.rs`).
pub(crate) fn spawn_netpol_keeper(
    api_url: &str,
    node_name: &str,
    // T1.3a (Q47 P3): the keeper's LISTs authenticate like the kubelet.
    token: Option<&str>,
    shutdown: Shutdown,
) -> Option<tokio::task::JoinHandle<()>> {
    let client = match HttpJson::parse_url(api_url) {
        Ok(c) => Arc::new(c.with_token(token.map(str::to_string))),
        Err(e) => {
            tracing::warn!(target: "init-pro", error = %e, "netpol keeper: bad api url");
            return None;
        }
    };
    let node = node_name.to_string();
    Some(tokio::spawn(async move {
        let mut tick = tokio::time::interval(INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = tick.tick() => {}
                () = shutdown.cancelled() => break,
            }
            match sync_once(&client, &node).await {
                Ok(chains) if chains > 0 => tracing::debug!(
                    target: "init-pro",
                    node = %node,
                    chains,
                    "network policy applied (T4.4)"
                ),
                Ok(_) => {}
                Err(e) => tracing::warn!(
                    target: "init-pro",
                    node = %node,
                    error = %e,
                    "network policy sync skipped (last program kept)"
                ),
            }
        }
        // NP4: delete the jump + every INIT-PRO-NPOL-* chain before exit.
        if let Err(e) = teardown().await {
            tracing::warn!(
                target: "init-pro",
                node = %node,
                error = %e,
                "network policy teardown failed (residue possible)"
            );
        } else {
            tracing::info!(target: "init-pro", node = %node, "network policy torn down");
        }
    }))
}

/// One resync: fetch the three lists, compile, apply, then hook FORWARD.
/// ORDER MATTERS: the restore payload DECLARES `INIT-PRO-NPOL-FWD`
/// (`:INIT-PRO-NPOL-FWD - [0:0]`), so it must land BEFORE
/// `iptables -I FORWARD -j INIT-PRO-NPOL-FWD` — the insert fails with
/// "Chain does not exist" when the chain is not yet in the table.
/// Returns the chain count for diagnostics.
async fn sync_once(client: &HttpJson, node: &str) -> Result<usize, String> {
    let (pods, policies, namespaces) = (
        get_list(client, "/api/v1/pods").await?,
        get_list(client, "/apis/networking.k8s.io/v1/networkpolicies").await?,
        get_list(client, "/api/v1/namespaces").await?,
    );
    let program = compile_netpol(&pods, &policies, &namespaces, node);
    apply_program(&program)
        .await
        .map_err(|e| format!("iptables-restore: {e}"))?;
    ensure_dispatch_jump()
        .await
        .map_err(|e| format!("dispatch jump: {e}"))?;
    Ok(program.chains.len())
}

/// GET a list path, requiring HTTP 200 and an `items` array.
async fn get_list(client: &HttpJson, path: &str) -> Result<Value, String> {
    let (status, body) = client
        .get_json(path)
        .await
        .map_err(|e| format!("GET {path}: {e}"))?;
    if status != 200 {
        return Err(format!("GET {path}: status {status}"));
    }
    if body.get("items").and_then(Value::as_array).is_none() {
        return Err(format!("GET {path}: malformed list body"));
    }
    Ok(body)
}
