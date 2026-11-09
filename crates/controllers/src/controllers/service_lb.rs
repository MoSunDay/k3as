//! ServiceLB announce controller (TODO **T4.3 Scope B / T5.6**, decision
//! **Q41**): the embedded klipper-lb equivalent.
//!
//! `type=LoadBalancer` Services already get nodePorts allocated by the
//! apiserver, but nothing announced `status.loadBalancer.ingress` -- this
//! controller closes that gap. VIP selection is k3s/klipper-lb semantics:
//! an explicit `spec.loadBalancerIP` wins; otherwise the first Node
//! InternalIP is the announce address (the ServiceLB data plane listens on
//! nodePorts on every node, so any node IP reaches the Service).
//!
//! Ownership discipline (never fight an existing LB manager): a Service
//! that already carries a NON-EMPTY `status.loadBalancer.ingress` array is
//! never rewritten -- an external MetalLB/cloud provider may own it. The
//! decision is pure ([`desired_ingress`]); the reconciler reads the
//! Service AND the Node list fresh through the [`Client`] (the queue key
//! may have been enqueued by a node event long after any cached copy went
//! stale), then CAS-writes the status onto a clone. A lost CAS surfaces as
//! [`ControllerError::Conflict`] and the worker requeues rate-limited
//! (the same shape as `node_cidr::allocate`). JSON-only wire (Q10).

use std::sync::Arc;

use serde_json::Value;
use storage::{Key, KeyPrefix};

use crate::client::Client;
use crate::error::ControllerError;
use crate::object::resource_version;

/// True when `/spec/type` == `LoadBalancer` -- the ServiceLB trigger.
/// Single-sourced here: the runner's Service/node informer fan-out uses the
/// same predicate to decide what reaches the `service_lbs` queue.
pub fn is_load_balancer(svc: &Value) -> bool {
    svc.pointer("/spec/type").and_then(Value::as_str) == Some("LoadBalancer")
}

/// The VIP this controller should announce, or `None` when it must NOT
/// write: non-LB Services; already-announced Services (non-empty
/// `status.loadBalancer.ingress` array -- never fight an existing LB
/// manager); LB Services with neither an explicit IP nor any node IP yet
/// (a late node event re-enqueues and completes the announce).
pub fn desired_ingress(svc: &Value, node_ips: &[String]) -> Option<String> {
    if !is_load_balancer(svc) {
        return None;
    }
    let announced = svc
        .pointer("/status/loadBalancer/ingress")
        .and_then(Value::as_array)
        .is_some_and(|a| !a.is_empty());
    if announced {
        return None;
    }
    match svc
        .pointer("/spec/loadBalancerIP")
        .and_then(Value::as_str)
        .filter(|ip| !ip.is_empty())
    {
        Some(ip) => Some(ip.to_string()),
        None => node_ips.first().cloned(),
    }
}

/// Every Node `status.addresses[]` entry of type `InternalIP`, in list
/// order (the wire shape kubelets write and the apiserver's pod IO reads;
/// klipper-lb announces the first one when no explicit IP is set). Nodes
/// without addresses / InternalIP entries are simply skipped.
pub fn node_internal_ips(nodes: &[Value]) -> Vec<String> {
    let mut ips = Vec::new();
    for node in nodes {
        let Some(addresses) = node.pointer("/status/addresses").and_then(Value::as_array) else {
            continue;
        };
        for address in addresses {
            if address.get("type").and_then(Value::as_str) == Some("InternalIP") {
                if let Some(ip) = address.get("address").and_then(Value::as_str) {
                    ips.push(ip.to_string());
                }
            }
        }
    }
    ips
}

/// Set `/status/loadBalancer/ingress = [{"ip": vip}]`, creating the
/// intermediate objects as needed. Returns false when the target is not a
/// JSON object (a malformed Service the caller surfaces as Internal).
fn set_ingress(svc: &mut Value, vip: &str) -> bool {
    let Some(root) = svc.as_object_mut() else {
        return false;
    };
    let status = root
        .entry("status")
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    let Some(status) = status.as_object_mut() else {
        return false;
    };
    let lb = status
        .entry("loadBalancer")
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    let Some(lb) = lb.as_object_mut() else {
        return false;
    };
    lb.insert(
        "ingress".into(),
        Value::Array(vec![serde_json::json!({"ip": vip})]),
    );
    true
}

/// Announce `status.loadBalancer.ingress` for one Service (`ns`/`name`).
/// Idempotent: `desired_ingress` gates every write, so announced / non-LB
/// / IP-less Services are no-ops (no rewrite, no churn).
pub async fn reconcile(
    client: &Arc<dyn Client>,
    ns: &str,
    name: &str,
) -> Result<(), ControllerError> {
    let key = Key::new("", "services", ns, name);
    let Some(svc) = client.get(&key).await? else {
        return Ok(()); // deleted concurrently: nothing to announce
    };
    // Node IPs come from a fresh cluster-scoped LIST (the same client seam
    // the podCIDR allocator uses), never from a possibly-stale cache: a
    // late kubelet InternalIP report must be visible on this pass.
    let nodes = client.list(&KeyPrefix::new("", "nodes", None)).await?;
    let Some(vip) = desired_ingress(&svc, &node_internal_ips(&nodes)) else {
        return Ok(());
    };
    let mut updated = svc.clone();
    if !set_ingress(&mut updated, &vip) {
        return Err(ControllerError::Internal(format!(
            "service {ns}/{name} is not a JSON object"
        )));
    }
    client
        .update(&key, updated, resource_version(&svc))
        .await
        .map_err(|e| {
            if e.is_conflict() {
                ControllerError::Conflict(format!("ServiceLB announce raced on {ns}/{name}"))
            } else {
                e
            }
        })?;
    tracing::debug!(service = name, namespace = ns, ip = %vip, "ServiceLB announced ingress");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::StorageClient;
    use serde_json::json;
    use storage::EmbeddedStorage;

    fn lb_service(load_balancer_ip: Option<&str>) -> Value {
        let mut svc = json!({
            "apiVersion": "v1", "kind": "Service",
            "metadata": {"name": "web", "namespace": "default"},
            "spec": {"type": "LoadBalancer", "ports": [
                {"port": 80, "targetPort": 8080},
            ]},
        });
        if let Some(ip) = load_balancer_ip {
            svc["spec"]["loadBalancerIP"] = json!(ip);
        }
        svc
    }

    fn node_with_ip(name: &str, ip: &str) -> Value {
        json!({
            "apiVersion": "v1", "kind": "Node",
            "metadata": {"name": name},
            "status": {"addresses": [
                {"type": "Hostname", "address": name},
                {"type": "InternalIP", "address": ip},
            ]},
        })
    }

    #[test]
    fn desired_ingress_ignores_non_lb_services() {
        assert_eq!(desired_ingress(&lb_service(None), &[]), None);
        for ty in ["ClusterIP", "NodePort", "ExternalName"] {
            let mut svc = lb_service(None);
            svc["spec"]["type"] = json!(ty);
            assert_eq!(desired_ingress(&svc, &[]), None, "type={ty}");
        }
        // Missing type entirely: not ours.
        let mut svc = lb_service(None);
        svc["spec"].as_object_mut().unwrap().remove("type");
        assert_eq!(desired_ingress(&svc, &[]), None);
    }

    #[test]
    fn desired_ingress_leaves_announced_services_alone() {
        let mut svc = lb_service(None);
        svc["status"] = json!({"loadBalancer": {"ingress": [{"ip": "198.51.100.9"}]}});
        // Even with a fresh explicit IP available: an existing LB manager
        // owns the announce; never fight it.
        svc["spec"]["loadBalancerIP"] = json!("203.0.113.7");
        assert_eq!(desired_ingress(&svc, &["192.168.1.10".into()]), None);
        // An EMPTY array is not an announce: still ours to write (the
        // explicit IP alone is a sufficient source, nodes or not).
        svc["status"] = json!({"loadBalancer": {"ingress": []}});
        assert_eq!(desired_ingress(&svc, &[]), Some("203.0.113.7".into()));
    }

    #[test]
    fn desired_ingress_prefers_explicit_ip() {
        let svc = lb_service(Some("203.0.113.7"));
        assert_eq!(
            desired_ingress(&svc, &["192.168.1.10".into()]),
            Some("203.0.113.7".into())
        );
        // Explicit IP wins even with no nodes at all.
        assert_eq!(desired_ingress(&svc, &[]), Some("203.0.113.7".into()));
        // An empty-string IP is treated as absent (upstream treats "" as
        // unset): fall back to the node list.
        let svc = lb_service(Some(""));
        assert_eq!(
            desired_ingress(&svc, &["192.168.1.10".into()]),
            Some("192.168.1.10".into())
        );
    }

    #[test]
    fn desired_ingress_falls_back_to_first_node_ip() {
        let svc = lb_service(None);
        assert_eq!(
            desired_ingress(&svc, &["192.168.1.10".into(), "192.168.1.11".into()]),
            Some("192.168.1.10".into())
        );
    }

    #[test]
    fn desired_ingress_without_any_ip_source_is_none() {
        assert_eq!(desired_ingress(&lb_service(None), &[]), None);
    }

    #[test]
    fn node_internal_ips_reads_internal_addresses_only() {
        let nodes = vec![
            node_with_ip("a", "192.168.1.10"),
            json!({"metadata": {"name": "b"}}), // no status yet
            node_with_ip("c", "192.168.1.12"),
        ];
        assert_eq!(
            node_internal_ips(&nodes),
            vec!["192.168.1.10".to_string(), "192.168.1.12".to_string()]
        );
    }

    #[tokio::test]
    async fn reconcile_announces_the_first_node_ip() {
        let store = Arc::new(EmbeddedStorage::new());
        let client: Arc<dyn Client> = Arc::new(StorageClient::new(store));
        client
            .create(
                &Key::new("", "nodes", "", "a"),
                node_with_ip("a", "192.168.1.10"),
            )
            .await
            .unwrap();
        client
            .create(
                &Key::new("", "nodes", "", "b"),
                node_with_ip("b", "192.168.1.11"),
            )
            .await
            .unwrap();
        client
            .create(
                &Key::new("", "services", "default", "web"),
                lb_service(None),
            )
            .await
            .unwrap();
        reconcile(&client, "default", "web").await.unwrap();
        let svc = client
            .get(&Key::new("", "services", "default", "web"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            svc.pointer("/status/loadBalancer/ingress/0/ip")
                .and_then(Value::as_str),
            Some("192.168.1.10")
        );
    }

    #[tokio::test]
    async fn reconcile_honors_explicit_load_balancer_ip() {
        let store = Arc::new(EmbeddedStorage::new());
        let client: Arc<dyn Client> = Arc::new(StorageClient::new(store));
        client
            .create(
                &Key::new("", "nodes", "", "a"),
                node_with_ip("a", "192.168.1.10"),
            )
            .await
            .unwrap();
        client
            .create(
                &Key::new("", "services", "default", "web"),
                lb_service(Some("203.0.113.7")),
            )
            .await
            .unwrap();
        reconcile(&client, "default", "web").await.unwrap();
        let svc = client
            .get(&Key::new("", "services", "default", "web"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            svc.pointer("/status/loadBalancer/ingress/0/ip")
                .and_then(Value::as_str),
            Some("203.0.113.7")
        );
    }

    #[tokio::test]
    async fn reconcile_never_fights_an_existing_ingress() {
        let store = Arc::new(EmbeddedStorage::new());
        let client: Arc<dyn Client> = Arc::new(StorageClient::new(store));
        let mut svc = lb_service(None);
        svc["status"] = json!({"loadBalancer": {"ingress": [{"ip": "198.51.100.9"}]}});
        client
            .create(&Key::new("", "services", "default", "web"), svc)
            .await
            .unwrap();
        client
            .create(
                &Key::new("", "nodes", "", "a"),
                node_with_ip("a", "192.168.1.10"),
            )
            .await
            .unwrap();
        reconcile(&client, "default", "web").await.unwrap();
        let svc = client
            .get(&Key::new("", "services", "default", "web"))
            .await
            .unwrap()
            .unwrap();
        // The external manager's announce is untouched.
        assert_eq!(
            svc.pointer("/status/loadBalancer/ingress/0/ip")
                .and_then(Value::as_str),
            Some("198.51.100.9")
        );
    }
}
