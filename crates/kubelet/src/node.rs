//! Node registration + heartbeat loops (TODO **T4.2**).
//!
//! Task 3 of [`crate::runner::spawn`]: ensure the Node object exists
//! (GET -> 404 -> POST, tolerating a concurrent 409), then every heartbeat
//! period renew the `kube-node-lease` Lease and refresh the node's Ready
//! status via a full PUT (resourceVersion preserved, so CAS; a 409 just
//! retries on the next tick). All failures are logged and retried.
//!
//! T4.2 hardening: the annotation `init-pro.io/kubelet-port` (the relay
//! discovery key, T4.2 Scope C / Q31) is only set at Node creation, so an
//! operator changing `--kubelet-port` and restarting the agent would leave
//! a stale port behind. The heartbeat therefore also reconciles that
//! annotation on every refresh via
//! [`reconciled_kubelet_port_annotation`] (no extra request: the corrected
//! map rides the same GET-modify-PUT as the status refresh, and the write
//! is skipped entirely when the annotation already matches).

use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

use crate::http::HttpJson;
use crate::objects;
use crate::runner::KubeletConfig;

/// Annotation key on the Node object carrying the local log/exec listener
/// port (frozen contract with `apiserver::pod_io`; NOTE: the key contains
/// '/', so never address it through a JSON pointer).
const KUBELET_PORT_ANNOTATION: &str = "init-pro.io/kubelet-port";

/// Task 3: ensure the Node object exists, then heartbeat the lease and the
/// node Ready status every heartbeat period.
pub(crate) async fn node_task(
    client: Arc<HttpJson>,
    cfg: KubeletConfig,
    shutdown: infra::Shutdown,
) {
    loop {
        match ensure_node(&client, &cfg).await {
            Ok(()) => break,
            Err(e) => {
                tracing::warn!(target: "init-pro", error = %e, "kubelet: node registration failed; retrying");
                tokio::select! {
                    _ = shutdown.cancelled() => return,
                    _ = tokio::time::sleep(Duration::from_secs(1)) => {}
                }
            }
        }
    }
    let mut tick = tokio::time::interval(cfg.heartbeat_period);
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => return,
            _ = tick.tick() => {}
        }
        heartbeat(&client, &cfg).await;
    }
}

async fn ensure_node(client: &HttpJson, cfg: &KubeletConfig) -> Result<(), String> {
    let name = cfg.node_name.as_str();
    let path = format!("/api/v1/nodes/{name}");
    match client.get_json(&path).await {
        Ok((200, _)) => Ok(()),
        Ok((404, _)) => {
            let body = objects::node_object(name, &cfg.node_ip, cfg.kubelet_port, &now_str());
            match client.post_json("/api/v1/nodes", &body).await {
                Ok((code, _)) if code == 200 || code == 201 => Ok(()),
                Ok((409, _)) => Ok(()), // created concurrently elsewhere
                Ok((code, resp)) => Err(format!("POST node -> HTTP {code}: {resp}")),
                Err(e) => Err(e.to_string()),
            }
        }
        Ok((code, resp)) => Err(format!("GET node -> HTTP {code}: {resp}")),
        Err(e) => Err(e.to_string()),
    }
}

/// One heartbeat: renew (or create) the node lease, refresh node status.
async fn heartbeat(client: &HttpJson, cfg: &KubeletConfig) {
    let name = cfg.node_name.as_str();
    let now = now_str();
    let lease_path =
        format!("/apis/coordination.k8s.io/v1/namespaces/kube-node-lease/leases/{name}");
    let lease = match client.get_json(&lease_path).await {
        Ok((200, l)) => Some(l),
        Ok((404, _)) => None,
        Ok((code, resp)) => {
            tracing::warn!(target: "init-pro", code = %code, %resp, "kubelet: lease GET failed");
            return;
        }
        Err(e) => {
            tracing::warn!(target: "init-pro", error = %e, "kubelet: lease GET failed");
            return;
        }
    };
    let write = match lease {
        Some(l) => {
            client
                .put_json(&lease_path, &objects::renew_lease(&l, &now))
                .await
        }
        None => {
            let path = "/apis/coordination.k8s.io/v1/namespaces/kube-node-lease/leases";
            client
                .post_json(
                    path,
                    &objects::new_lease("kube-node-lease", name, name, &now),
                )
                .await
        }
    };
    match write {
        Ok((code, _)) if code == 200 || code == 201 || code == 409 => {}
        Ok((code, resp)) => {
            tracing::warn!(target: "init-pro", code = %code, %resp, "kubelet: lease write rejected");
        }
        Err(e) => tracing::warn!(target: "init-pro", error = %e, "kubelet: lease write failed"),
    }
    refresh_node_status(client, cfg, &now).await;
}

/// GET the node, replace `.status` with fresh Ready/capacity, full PUT back
/// (resourceVersion preserved -> CAS; a 409 just retries next tick). The
/// same PUT also converges a stale kubelet-port annotation (T4.2 hardening).
async fn refresh_node_status(client: &HttpJson, cfg: &KubeletConfig, now: &str) {
    let name = cfg.node_name.as_str();
    let path = format!("/api/v1/nodes/{name}");
    let Ok((200, mut node)) = client.get_json(&path).await else {
        return;
    };
    let Some(status) = objects::node_object(name, &cfg.node_ip, cfg.kubelet_port, now)
        .pointer("/status")
        .cloned()
    else {
        return;
    };
    if let Some(annotations) = reconciled_kubelet_port_annotation(&node, cfg.kubelet_port) {
        tracing::info!(
            target: "init-pro",
            node = name,
            port = cfg.kubelet_port,
            "kubelet: node kubelet-port annotation stale; reconciling"
        );
        node["metadata"]["annotations"] = annotations;
    }
    node["status"] = status;
    match client.put_json(&path, &node).await {
        Ok((200, _)) => {}
        Ok((code, resp)) => {
            tracing::debug!(target: "init-pro", code = %code, %resp, "kubelet: node refresh rejected");
        }
        Err(e) => tracing::debug!(target: "init-pro", error = %e, "kubelet: node refresh failed"),
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub(crate) fn now_str() -> String {
    common::time::now_rfc3339(unix_now())
}

/// Pure decision for kubelet-port annotation convergence (T4.2 hardening):
/// does the stored Node need a new `metadata.annotations`, and if so what
/// should the corrected map be?
///
/// Returns `Some(annotations)` — every pre-existing annotation preserved,
/// [`KUBELET_PORT_ANNOTATION`] set to `kubelet_port` — when the stored
/// annotation is missing, non-string or stale. Returns `None` when it
/// already matches, so the heartbeat skips the write and causes no extra
/// CAS/resourceVersion churn on the happy path.
pub(crate) fn reconciled_kubelet_port_annotation(node: &Value, kubelet_port: u16) -> Option<Value> {
    // `get` chains, not a pointer: the annotation key contains '/'.
    let stored = node
        .get("metadata")
        .and_then(|m| m.get("annotations"))
        .and_then(|a| a.get(KUBELET_PORT_ANNOTATION))
        .and_then(Value::as_str);
    let wanted = kubelet_port.to_string();
    if stored == Some(wanted.as_str()) {
        return None;
    }
    let mut annotations = node
        .get("metadata")
        .and_then(|m| m.get("annotations"))
        .filter(|a| a.is_object())
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    annotations[KUBELET_PORT_ANNOTATION] = Value::String(wanted);
    Some(annotations)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn node_with_annotations(annotations: Value) -> Value {
        json!({
            "apiVersion": "v1",
            "kind": "Node",
            "metadata": {"name": "n1", "resourceVersion": "9", "annotations": annotations},
            "spec": {},
            "status": {},
        })
    }

    #[test]
    fn kubelet_port_annotation_already_correct_no_update() {
        let node = node_with_annotations(json!({"init-pro.io/kubelet-port": "10250"}));
        assert_eq!(reconciled_kubelet_port_annotation(&node, 10250), None);
    }

    #[test]
    fn kubelet_port_annotation_stale_or_missing_produces_update() {
        // Stale port (operator changed --kubelet-port, agent restarted).
        let node = node_with_annotations(json!({"init-pro.io/kubelet-port": "10250"}));
        assert_eq!(
            reconciled_kubelet_port_annotation(&node, 10255),
            Some(json!({"init-pro.io/kubelet-port": "10255"}))
        );
        // Annotation missing entirely (hand-written Node fixture).
        let node = json!({
            "apiVersion": "v1",
            "kind": "Node",
            "metadata": {"name": "n1"},
        });
        assert_eq!(
            reconciled_kubelet_port_annotation(&node, 10250),
            Some(json!({"init-pro.io/kubelet-port": "10250"}))
        );
        // Non-string value counts as stale, not as a match.
        let node = node_with_annotations(json!({"init-pro.io/kubelet-port": 10250}));
        assert_eq!(
            reconciled_kubelet_port_annotation(&node, 10250),
            Some(json!({"init-pro.io/kubelet-port": "10250"}))
        );
    }

    #[test]
    fn kubelet_port_annotation_preserves_other_annotations() {
        let node = node_with_annotations(json!({
            "init-pro.io/kubelet-port": "10250",
            "flannel.alpha/coreos.com/public-ip": "10.0.0.7",
            "another/example": "keep-me",
        }));
        assert_eq!(
            reconciled_kubelet_port_annotation(&node, 10255),
            Some(json!({
                "init-pro.io/kubelet-port": "10255",
                "flannel.alpha/coreos.com/public-ip": "10.0.0.7",
                "another/example": "keep-me",
            }))
        );
    }
}
