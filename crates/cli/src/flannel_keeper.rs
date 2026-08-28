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
/// real kubelets merge node conditions by `type`. Two init-pro specifics
/// make a keeper necessary:
/// - the kubelet node heartbeat (T4.2) rewrites `status.conditions`
///   wholesale, so flanneld's condition would vanish within one beat;
/// - our apiserver strategic-merge has no merge key for `status/conditions`,
///   so flanneld's own PATCH replaces the array (dropping `Ready`).
///
/// The keeper therefore does GET -> upsert-by-type -> PUT (CAS), preserving
/// every foreign condition, and no-ops while the condition already reads
/// `False`. It stops as soon as the daemon task exits (a dead flanneld must
/// not claim the network is up); agent shutdown cancels the daemon first.
pub(crate) fn spawn_condition_keeper(
    api_url: &str,
    node_name: &str,
    daemon_alive: Arc<AtomicBool>,
) {
    let client = match kubelet::HttpJson::parse_url(api_url) {
        Ok(c) => std::sync::Arc::new(c),
        Err(e) => {
            tracing::warn!(target: "init-pro", error = %e, "flannel condition keeper: bad api url");
            return;
        }
    };
    let node = node_name.to_string();
    tokio::spawn(async move {
        let path = format!("/api/v1/nodes/{node}");
        loop {
            // 5s: same cadence as flanneld's iptables resync; a beat is one
            // cheap GET while the condition is already False.
            tokio::time::sleep(Duration::from_secs(5)).await;
            if !daemon_alive.load(Ordering::Relaxed) {
                return;
            }
            let Ok((200, mut node_obj)) = client.get_json(&path).await else {
                continue;
            };
            if !condition_reads_false(&node_obj) {
                upsert_network_unavailable(&mut node_obj);
                match client.put_json(&path, &node_obj).await {
                    Ok((200 | 201 | 409, _)) => {}
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

/// Upsert the flannel condition by `type`, preserving all other conditions
/// and the rest of the object (annotations, capacity, addresses, ...).
fn upsert_network_unavailable(node: &mut serde_json::Value) {
    let now = common::time::now_rfc3339(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    );
    let cond = serde_json::json!({
        "type": "NetworkUnavailable",
        "status": "False",
        "reason": "FlannelIsUp",
        "message": "Flannel is running on this node",
        "lastTransitionTime": now,
        "lastHeartbeatTime": now,
    });
    let obj = match node.as_object_mut() {
        Some(o) => o,
        None => return,
    };
    let status = obj.entry("status").or_insert_with(|| serde_json::json!({}));
    let sobj = match status.as_object_mut() {
        Some(o) => o,
        None => return,
    };
    match sobj.get_mut("conditions").and_then(|c| c.as_array_mut()) {
        Some(conds) => match conds
            .iter_mut()
            .find(|c| c.get("type").and_then(serde_json::Value::as_str) == Some("NetworkUnavailable"))
        {
            Some(slot) => *slot = cond,
            None => conds.push(cond),
        },
        None => {
            sobj.insert("conditions".into(), serde_json::json!([cond]));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;


    #[test]
    fn network_unavailable_upsert_preserves_foreign_conditions() {
        // A kubelet-shaped node: Ready only, no flannel condition yet.
        let mut node = serde_json::json!({
            "apiVersion": "v1",
            "kind": "Node",
            "metadata": {"name": "fn-node", "annotations": {"flannel.alpha.coreos.com/backend-type": "vxlan"}},
            "status": {
                "conditions": [{
                    "type": "Ready", "status": "True", "reason": "KubeletReady",
                    "lastHeartbeatTime": "t0", "lastTransitionTime": "t0"
                }],
                "addresses": [{"type": "InternalIP", "address": "127.0.0.1"}]
            }
        });
        assert!(!condition_reads_false(&node));
        upsert_network_unavailable(&mut node);

        let conds = node.pointer("/status/conditions").unwrap().as_array().unwrap();
        assert_eq!(conds.len(), 2, "Ready must survive the upsert");
        let ready = conds.iter().find(|c| c["type"] == "Ready").unwrap();
        assert_eq!(ready["status"], "True", "Ready untouched");
        let nu = conds.iter().find(|c| c["type"] == "NetworkUnavailable").unwrap();
        assert_eq!(nu["status"], "False");
        assert_eq!(nu["reason"], "FlannelIsUp");
        assert_eq!(
            node["metadata"]["annotations"]["flannel.alpha.coreos.com/backend-type"],
            serde_json::json!("vxlan")
        );
        assert!(condition_reads_false(&node));

        // Idempotent: a second upsert replaces in place (still 2 conditions).
        upsert_network_unavailable(&mut node);
        assert_eq!(
            node.pointer("/status/conditions").unwrap().as_array().unwrap().len(),
            2
        );

        // A node with no status at all gains the condition cleanly.
        let mut bare = serde_json::json!({"kind": "Node", "metadata": {"name": "n"}});
        upsert_network_unavailable(&mut bare);
        assert!(condition_reads_false(&bare));
    }
}
