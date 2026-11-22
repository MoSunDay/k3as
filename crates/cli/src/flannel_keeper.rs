//! Node `NetworkUnavailable` condition keeper for the embedded flanneld
//! (TODO **T4.3 Scope A**, decision **Q34**).
//!
//! Split out of [`crate::flannel`] to keep both files under the 400-line
//! cap: the keeper is a self-contained kube-watching concern, the rest of
//! flannel wiring is net-conf + daemon spawn.
#![forbid(unsafe_code)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Keep `NetworkUnavailable=False` on this node while flanneld runs (Q34).
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
/// ...) survive without any client-side merge. The keeper still no-ops
/// while the condition already reads `False` (one cheap GET per beat) and
/// stops as soon as the daemon task exits (a dead flanneld must not claim
/// the network is up); agent shutdown cancels the daemon first.
pub(crate) fn spawn_condition_keeper(
    api_url: &str,
    node_name: &str,
    // T1.3a (Q47 P3): the keeper's GET/PATCH authenticate like the kubelet.
    token: Option<&str>,
    daemon_alive: Arc<AtomicBool>,
) {
    let client = match kubelet::HttpJson::parse_url(api_url) {
        Ok(c) => std::sync::Arc::new(c.with_token(token.map(str::to_string))),
        Err(e) => {
            tracing::warn!(target: "init-pro", error = %e, "flannel condition keeper: bad api url");
            return;
        }
    };
    let node = node_name.to_string();
    tokio::spawn(async move {
        let path = format!("/api/v1/nodes/{node}");
        let status_path = format!("{path}/status");
        loop {
            // 5s: same cadence as flanneld's iptables resync; a beat is one
            // cheap GET while the condition is already False.
            tokio::time::sleep(Duration::from_secs(5)).await;
            if !daemon_alive.load(Ordering::Relaxed) {
                return;
            }
            let Ok((200, node_obj)) = client.get_json(&path).await else {
                continue;
            };
            if !condition_reads_false(&node_obj) {
                // One-condition strategic patch: the apiserver merges the
                // conditions array by `type` (S51/D7), so `Ready` & co. are
                // preserved server-side and we never ship the whole object.
                let patch = serde_json::json!({
                    "status": { "conditions": [network_unavailable_cond()] }
                });
                match client.patch_json_strategic(&status_path, &patch).await {
                    Ok((200 | 201, _)) => {}
                    Ok((code, resp)) => tracing::debug!(
                        target: "init-pro", code, %resp, "flannel: NetworkUnavailable refresh rejected"
                    ),
                    Err(e) => tracing::debug!(
                        target: "init-pro", error = %e, "flannel: NetworkUnavailable refresh failed"
                    ),
                }
            }
        }
    });
}

/// True when `status.conditions` already carries `NetworkUnavailable=False`.
fn condition_reads_false(node: &serde_json::Value) -> bool {
    node.pointer("/status/conditions")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .any(|c| {
            c.get("type").and_then(serde_json::Value::as_str) == Some("NetworkUnavailable")
                && c.get("status").and_then(serde_json::Value::as_str) == Some("False")
        })
}

/// The flannel `NetworkUnavailable=False` condition body (S51/D7: a bare
/// builder — the by-`type` array merge now lives in the apiserver).
fn network_unavailable_cond() -> serde_json::Value {
    let now = common::time::now_rfc3339(
        std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    );
    serde_json::json!({
        "type": "NetworkUnavailable",
        "status": "False",
        "reason": "FlannelIsUp",
        "message": "Flannel is running on this node",
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
}
