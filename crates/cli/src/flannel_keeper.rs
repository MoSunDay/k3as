//! Node `NetworkUnavailable` condition keeper for the embedded flanneld
//! (TODO **T4.3 Scope A**, decision **Q34**).
//!
//! Split out of [`crate::flannel`] to keep both files under the 400-line
//! cap: the keeper is a self-contained kube-watching concern, the rest of
//! flannel wiring is net-conf + daemon spawn.
//!
//! C8 (long-run bug sweep): the keeper is honest in BOTH directions and
//! shutdown-aware. It used to only ever clear the condition (patch
//! `False` while the daemon ran) and to return the moment the daemon
//! died, which left a stale `NetworkUnavailable=False` on a node with no
//! pod networking. It now patches `True` (reason `FlannelExited`) while
//! the daemon is down, keeps looping so a restarted daemon flips the
//! condition back, slows down after repeated patch failures, and stops
//! only on agent shutdown.
#![forbid(unsafe_code)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use infra::Shutdown;
use kubelet::HttpJson;
use serde_json::Value;

/// Keeper cadence while its beats succeed: same as flanneld's iptables
/// resync; a beat is one cheap GET while the condition already agrees.
const KEEPER_PERIOD: Duration = Duration::from_secs(5);
/// C8: slowed cadence once a direction failed [`SLOW_AFTER_FAILURES`]
/// beats in a row, so a down apiserver is not hammered every 5 s.
const KEEPER_SLOW_PERIOD: Duration = Duration::from_secs(30);
/// C8: consecutive failures (per direction) that trip the slow period.
const SLOW_AFTER_FAILURES: u32 = 3;
/// C8: bound on the immediate network-down patch issued from the restart
/// loop - a wedged apiserver must never stall the flanneld retry.
const NETWORK_DOWN_PATCH_TIMEOUT: Duration = Duration::from_secs(10);

/// Keep the node's `NetworkUnavailable` condition truthful (Q34, C8).
///
/// Upstream flannel clears the condition once, at `CompleteLease` time, and
/// real kubelets merge node conditions by `type`. The kubelet node heartbeat
/// (T4.2) rewrites `status.conditions` wholesale, so flanneld's condition
/// would vanish within one beat — hence a keeper.
///
/// S51 (D7): the merge-key debt is gone. The apiserver's
/// `status/conditions` strategic-merge key is `type` (upstream parity), so
/// the keeper PATCHes the node `/status` subresource with a one-condition
/// body and the server upserts it in place — foreign conditions (`Ready`,
/// ...) survive without any client-side merge. The keeper no-ops while the
/// condition already agrees with the daemon (one cheap GET per beat).
///
/// C8: while the daemon is alive it patches `NetworkUnavailable=False`;
/// while the daemon is NOT alive it patches `NetworkUnavailable=True`
/// (reason `FlannelExited`) and keeps looping, so a restarted daemon flips
/// the condition back to `False`. The task returns only on `shutdown`.
/// Each direction tracks its own consecutive patch failures and the loop
/// slows to [`KEEPER_SLOW_PERIOD`] after [`SLOW_AFTER_FAILURES`] of them
/// (reset on success). Agent shutdown cancels the daemon first; this task
/// is detached, so the token is what keeps it from outliving the agent.
pub(crate) fn spawn_condition_keeper(
    api_url: &str,
    node_name: &str,
    // T1.3a (Q47 P3): the keeper's GET/PATCH authenticate like the kubelet.
    token: Option<&str>,
    daemon_alive: Arc<AtomicBool>,
    // C8: the keeper now exits only on shutdown, so it needs the token.
    shutdown: Shutdown,
) {
    let client = match HttpJson::parse_url(api_url) {
        Ok(c) => Arc::new(c.with_token(token.map(str::to_string))),
        Err(e) => {
            tracing::warn!(target: "init-pro", error = %e, "flannel condition keeper: bad api url");
            return;
        }
    };
    let node = node_name.to_string();
    tokio::spawn(async move {
        let path = format!("/api/v1/nodes/{node}");
        let status_path = format!("{path}/status");
        // C8: one consecutive-failure streak per direction.
        let mut up_failures: u32 = 0;
        let mut down_failures: u32 = 0;
        loop {
            let alive = daemon_alive.load(Ordering::Relaxed);
            let period = keeper_period(if alive { up_failures } else { down_failures });
            tokio::select! {
                _ = shutdown.cancelled() => return,
                _ = tokio::time::sleep(period) => {}
            }
            // Re-read after the sleep: the daemon may have flipped while
            // the beat was pending (restart loop, C8).
            let alive = daemon_alive.load(Ordering::Relaxed);
            let ok = keeper_beat(&client, &path, &status_path, alive)
                .await
                .is_ok();
            if alive {
                up_failures = next_failures(up_failures, ok);
            } else {
                down_failures = next_failures(down_failures, ok);
            }
        }
    });
}

/// C8: one keeper beat. `Ok(())` = the node condition already agrees with
/// `daemon_alive` (or was just fixed); `Err(())` = the beat failed and
/// counts toward the active direction's backoff.
async fn keeper_beat(
    client: &HttpJson,
    path: &str,
    status_path: &str,
    daemon_alive: bool,
) -> Result<(), ()> {
    let Ok((200, node_obj)) = client.get_json(path).await else {
        return Err(());
    };
    let now = unix_now_secs();
    let (agrees, cond) = if daemon_alive {
        (condition_reads_false(&node_obj), network_unavailable_cond())
    } else {
        (
            condition_reads_true(&node_obj),
            network_unavailable_down_cond(now),
        )
    };
    if agrees {
        return Ok(());
    }
    if !daemon_alive {
        tracing::warn!(
            target: "init-pro",
            "flanneld is down; marking NetworkUnavailable=True (C8)"
        );
    }
    // One-condition strategic patch: the apiserver merges the conditions
    // array by `type` (S51/D7), so `Ready` & co. are preserved server-side
    // and we never ship the whole object.
    let patch = serde_json::json!({
        "status": { "conditions": [cond] }
    });
    match client.patch_json_strategic(status_path, &patch).await {
        Ok((200 | 201, _)) => Ok(()),
        Ok((code, resp)) => {
            tracing::debug!(
                target: "init-pro", code, %resp, "flannel: NetworkUnavailable refresh rejected"
            );
            Err(())
        }
        Err(e) => {
            tracing::debug!(
                target: "init-pro", error = %e, "flannel: NetworkUnavailable refresh failed"
            );
            Err(())
        }
    }
}

/// C8: best-effort immediate `NetworkUnavailable=True` (reason
/// `FlannelExited`) patch, issued by the supervised restart loop the
/// moment flanneld exits. The keeper owns the steady state; this closes
/// the up-to-one-beat window before its next tick so the scheduler stops
/// placing pods on a node with no pod networking. Bounded by
/// [`NETWORK_DOWN_PATCH_TIMEOUT`] and every failure is logged + ignored
/// (the keeper retries) - it must never break the restart loop.
pub(crate) async fn mark_network_down(api_url: &str, node_name: &str, token: Option<&str>) {
    let client = match HttpJson::parse_url(api_url) {
        Ok(c) => c.with_token(token.map(str::to_string)),
        Err(e) => {
            tracing::debug!(
                target: "init-pro",
                error = %e,
                "flannel: network-down patch skipped (bad api url)"
            );
            return;
        }
    };
    let status_path = format!("/api/v1/nodes/{node_name}/status");
    let patch = serde_json::json!({
        "status": { "conditions": [network_unavailable_down_cond(unix_now_secs())] }
    });
    let attempt = client.patch_json_strategic(&status_path, &patch);
    match tokio::time::timeout(NETWORK_DOWN_PATCH_TIMEOUT, attempt).await {
        Ok(Ok((200 | 201, _))) => {}
        Ok(Ok((code, resp))) => {
            tracing::debug!(target: "init-pro", code, %resp, "flannel: network-down patch rejected")
        }
        Ok(Err(e)) => {
            tracing::debug!(target: "init-pro", error = %e, "flannel: network-down patch failed")
        }
        Err(_) => {
            tracing::debug!(target: "init-pro", "flannel: network-down patch timed out")
        }
    }
}

/// C8: keeper cadence for one direction's consecutive-failure streak:
/// [`KEEPER_PERIOD`] while beats land, [`KEEPER_SLOW_PERIOD`] from
/// [`SLOW_AFTER_FAILURES`] failures on (the caller resets on success).
fn keeper_period(consecutive_failures: u32) -> Duration {
    if consecutive_failures >= SLOW_AFTER_FAILURES {
        KEEPER_SLOW_PERIOD
    } else {
        KEEPER_PERIOD
    }
}

/// C8: consecutive-failure counter update for one direction: reset on
/// success, grow (saturating) on a failed beat.
fn next_failures(prev: u32, ok: bool) -> u32 {
    if ok {
        0
    } else {
        prev.saturating_add(1)
    }
}

/// Unix seconds for the condition clock fields (0 if the clock is
/// somehow before the epoch, matching the other timestamp call sites).
fn unix_now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// True when `status.conditions` carries `NetworkUnavailable=<status>`.
fn condition_is(node: &Value, status: &str) -> bool {
    node.pointer("/status/conditions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .any(|c| {
            c.get("type").and_then(Value::as_str) == Some("NetworkUnavailable")
                && c.get("status").and_then(Value::as_str) == Some(status)
        })
}

/// True when `status.conditions` already carries `NetworkUnavailable=False`.
fn condition_reads_false(node: &Value) -> bool {
    condition_is(node, "False")
}

/// C8: True when `status.conditions` already carries
/// `NetworkUnavailable=True` (the write-if-changed guard for the down
/// patch: no PATCH churn while the node already reads down).
fn condition_reads_true(node: &Value) -> bool {
    condition_is(node, "True")
}

/// The flannel `NetworkUnavailable=False` condition body (S51/D7: a bare
/// builder - the by-`type` array merge now lives in the apiserver).
fn network_unavailable_cond() -> Value {
    network_unavailable_cond_at(unix_now_secs())
}

/// `NetworkUnavailable=False` stamped at `now` (C8: the clock is a
/// parameter so the body stays pure and testable).
fn network_unavailable_cond_at(now: u64) -> Value {
    network_unavailable_status_cond(
        "False",
        "FlannelIsUp",
        "Flannel is running on this node",
        now,
    )
}

/// C8: the `NetworkUnavailable=True` condition body - flanneld is NOT
/// running on this node, so the scheduler must stop placing pods here.
/// `now` is unix seconds (both clock fields are stamped with it).
fn network_unavailable_down_cond(now: u64) -> Value {
    network_unavailable_status_cond(
        "True",
        "FlannelExited",
        "Flannel is not running on this node",
        now,
    )
}

/// One `NetworkUnavailable` condition with both clock fields stamped.
fn network_unavailable_status_cond(status: &str, reason: &str, message: &str, now: u64) -> Value {
    let now = common::time::now_rfc3339(now);
    serde_json::json!({
        "type": "NetworkUnavailable",
        "status": status,
        "reason": reason,
        "message": message,
        "lastTransitionTime": now,
        "lastHeartbeatTime": now,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn network_unavailable_cond_shape() {
        let cond = network_unavailable_cond();
        assert_eq!(cond["type"], "NetworkUnavailable");
        assert_eq!(cond["status"], "False");
        assert_eq!(cond["reason"], "FlannelIsUp");
        // Timestamps are stamped on both clock fields (upstream parity).
        assert!(!cond["lastTransitionTime"].as_str().unwrap().is_empty());
        assert_eq!(cond["lastHeartbeatTime"], cond["lastTransitionTime"]);
    }

    #[test]
    fn c8_network_unavailable_down_cond_shape() {
        // C8: the down condition is status True / reason FlannelExited and
        // stamps BOTH clock fields from the caller-supplied `now`.
        let cond = network_unavailable_down_cond(1_700_000_000);
        assert_eq!(cond["type"], "NetworkUnavailable");
        assert_eq!(cond["status"], "True");
        assert_eq!(cond["reason"], "FlannelExited");
        assert_eq!(cond["message"], "Flannel is not running on this node");
        assert_eq!(cond["lastTransitionTime"], "2023-11-14T22:13:20Z");
        assert_eq!(cond["lastHeartbeatTime"], cond["lastTransitionTime"]);
        // The up builder keeps its own identity next to the down one.
        let up = network_unavailable_cond_at(1_700_000_000);
        assert_eq!(up["status"], "False");
        assert_eq!(up["reason"], "FlannelIsUp");
    }

    #[test]
    fn condition_reads_false_detection() {
        // False present -> true; True/absent/foreign types -> false.
        let with_false = serde_json::json!({"status": {"conditions": [
            {"type": "Ready", "status": "True"},
            {"type": "NetworkUnavailable", "status": "False"}
        ]}});
        assert!(condition_reads_false(&with_false));
        let with_true = serde_json::json!({"status": {"conditions": [
            {"type": "NetworkUnavailable", "status": "True"}
        ]}});
        assert!(!condition_reads_false(&with_true));
        assert!(!condition_reads_false(&serde_json::json!({
            "status": {"conditions": [{"type": "Ready", "status": "True"}]}
        })));
        assert!(!condition_reads_false(&serde_json::json!({"status": {}})));
        assert!(!condition_reads_false(&serde_json::json!({"kind": "Node"})));
    }

    #[test]
    fn c8_condition_reads_true_detection() {
        // C8: the down direction's write-if-changed guard.
        let with_true = serde_json::json!({"status": {"conditions": [
            {"type": "Ready", "status": "True"},
            {"type": "NetworkUnavailable", "status": "True"}
        ]}});
        assert!(condition_reads_true(&with_true));
        // A foreign condition with status True does not count.
        let foreign = serde_json::json!({"status": {"conditions": [
            {"type": "Ready", "status": "True"}
        ]}});
        assert!(!condition_reads_true(&foreign));
        // False / absent / no status at all -> not down.
        let with_false = serde_json::json!({"status": {"conditions": [
            {"type": "NetworkUnavailable", "status": "False"}
        ]}});
        assert!(!condition_reads_true(&with_false));
        assert!(!condition_reads_true(&serde_json::json!({"status": {}})));
        assert!(!condition_reads_true(&serde_json::json!({"kind": "Node"})));
    }

    #[test]
    fn c8_keeper_period_slows_after_three_failures() {
        // C8: 5 s while patches land, 30 s from the 3rd consecutive
        // failure on (and it stays 30 s, never grows further).
        assert_eq!(keeper_period(0), Duration::from_secs(5));
        assert_eq!(keeper_period(2), Duration::from_secs(5));
        assert_eq!(keeper_period(3), Duration::from_secs(30));
        assert_eq!(keeper_period(99), Duration::from_secs(30));
    }

    #[test]
    fn c8_next_failures_resets_on_success() {
        // C8: per-direction streak - success resets, failure grows and
        // saturates instead of wrapping back into the fast period.
        assert_eq!(next_failures(0, true), 0);
        assert_eq!(next_failures(7, true), 0);
        assert_eq!(next_failures(0, false), 1);
        assert_eq!(next_failures(2, false), 3);
        assert_eq!(next_failures(u32::MAX, false), u32::MAX);
    }
}
