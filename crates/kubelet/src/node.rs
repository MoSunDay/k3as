//! Node registration + heartbeat loops (TODO **T4.2**).
//!
//! Task 3 of [`crate::runner::spawn`]: ensure the Node object exists
//! (GET -> 404 -> POST, tolerating a concurrent 409), then every heartbeat
//! period renew the `kube-node-lease` Lease and refresh the node's Ready
//! status via a full PUT (resourceVersion preserved, so CAS; a 409 just
//! retries on the next tick). All failures are logged and retried.

use std::sync::Arc;
use std::time::Duration;

use crate::http::HttpJson;
use crate::objects;
use crate::runner::KubeletConfig;

/// Task 3: ensure the Node object exists, then heartbeat the lease and the
/// node Ready status every heartbeat period.
pub(crate) async fn node_task(
    client: Arc<HttpJson>,
    cfg: KubeletConfig,
    shutdown: infra::Shutdown,
) {
    loop {
        match ensure_node(&client, &cfg.node_name).await {
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
        heartbeat(&client, &cfg.node_name).await;
    }
}

async fn ensure_node(client: &HttpJson, name: &str) -> Result<(), String> {
    let path = format!("/api/v1/nodes/{name}");
    match client.get_json(&path).await {
        Ok((200, _)) => Ok(()),
        Ok((404, _)) => {
            let body = objects::node_object(name, &now_str());
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
async fn heartbeat(client: &HttpJson, name: &str) {
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
    refresh_node_status(client, name, &now).await;
}

/// GET the node, replace `.status` with fresh Ready/capacity, full PUT back
/// (resourceVersion preserved -> CAS; a 409 just retries next tick).
async fn refresh_node_status(client: &HttpJson, name: &str, now: &str) {
    let path = format!("/api/v1/nodes/{name}");
    let Ok((200, mut node)) = client.get_json(&path).await else {
        return;
    };
    let Some(status) = objects::node_object(name, now).pointer("/status").cloned() else {
        return;
    };
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
