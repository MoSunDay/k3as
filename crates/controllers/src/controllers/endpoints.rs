//! Endpoints controller (T3.1a).
//!
//! Reflects Service selector membership into an Endpoints object. As of Sprint
//! 18 the kubelet reports real podIPs (S1), so addresses carry the real CNI
//! IPs when present; the deterministic `10.42.x.y` placeholder derived from
//! the pod identity remains only for kubelet-less clusters (golden
//! conformance). Readiness: pods without ANY conditions count as ready --
//! without kubelet nothing would ever report Ready and Endpoints would stay
//! empty.

use serde_json::{json, Value};
use storage::Key;

use crate::client::Client;
use crate::controllers::is_terminating;
use crate::error::ControllerError;
use crate::id::placeholder_pod_ip;
use crate::object::{name, namespace, pod_is_ready, selector_matches, semantic_eq};

/// Reconcile one Service's Endpoints. `endpoints` is the existing object, if
/// any (caller reads it fresh); the write happens only on change.
pub async fn reconcile(
    client: &std::sync::Arc<dyn Client>,
    service: &Value,
    endpoints: Option<&Value>,
) -> Result<(), ControllerError> {
    // Terminating Services (deletionTimestamp set, T3.1b/Q20) stop
    // receiving endpoint updates; the namespace drain / GC owns teardown.
    if is_terminating(service) {
        return Ok(());
    }
    let ns = namespace(service).unwrap_or("default");
    let Some(svc_name) = name(service) else {
        return Ok(());
    };
    let selector = service.pointer("/spec/selector");
    // Selector-less services (absent or empty selector) are never managed:
    // upstream parity (empty selectors select nothing; such services carry
    // hand-curated or headless endpoints).
    let Some(sel) = selector.filter(|s| !selector_is_empty(s)) else {
        return Ok(());
    };
    let selector = sel.clone();

    let pods = client
        .list(&storage::KeyPrefix::new("", "pods", Some(ns.to_string())))
        .await?;
    let mut ready = Vec::new();
    let mut not_ready = Vec::new();
    for pod in pods
        .iter()
        .filter(|p| !is_terminating(p) && selector_matches(p, &selector))
    {
        let pod_name = name(pod).unwrap_or_default();
        let identity = pod
            .pointer("/metadata/uid")
            .and_then(Value::as_str)
            .filter(|u| !u.is_empty())
            .unwrap_or(pod_name);
        // Real kubelet-reported podIP wins (Sprint 18 / S1); the
        // deterministic placeholder is only for kubelet-less clusters.
        let ip = pod
            .pointer("/status/podIP")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| placeholder_pod_ip(identity));
        let addr = json!({"ip": ip, "hostname": pod_name});
        if pod_is_ready(pod) {
            ready.push(addr);
        } else {
            not_ready.push(addr);
        }
    }

    // k8s Endpoints semantics: `port` is the resolved container port
    // (targetPort), not the Service port; `name` carries the Service port
    // name. Sprint 18 / S2: previously the targetPort was dropped, which
    // forwarded every non-identity Service to the wrong port.
    let ports: Vec<Value> = service
        .pointer("/spec/ports")
        .and_then(Value::as_array)
        .map(|ps| {
            ps.iter()
                .filter_map(|p| {
                    let port = resolve_target_port(p, &pods)?;
                    let mut out = json!({
                        "port": port,
                        "protocol": p.get("protocol").cloned().unwrap_or(json!("TCP")),
                    });
                    if let Some(n) = p.get("name") {
                        out["name"] = n.clone();
                    }
                    Some(out)
                })
                .collect()
        })
        .unwrap_or_default();

    let subsets = if ready.is_empty() && not_ready.is_empty() {
        vec![]
    } else {
        vec![json!({"addresses": ready, "notReadyAddresses": not_ready, "ports": ports})]
    };
    let desired = json!({
        "apiVersion": "v1",
        "kind": "Endpoints",
        "metadata": {"name": svc_name, "namespace": ns},
        "subsets": subsets,
    });

    let key = Key::new("", "endpoints", ns, svc_name);
    match endpoints {
        None => match client.create(&key, desired).await {
            Ok(_) => tracing::debug!(service = svc_name, "created endpoints"),
            Err(e) if e.is_already_exists() => {}
            Err(e) => return Err(e),
        },
        Some(existing) => {
            if !semantic_eq(existing, &desired) {
                // Conflict propagates: the worker retries (no re-delivery
                // is guaranteed on a quiesced object).
                client
                    .update(&key, desired, resource_version_of(existing))
                    .await?;
                tracing::debug!(service = svc_name, "updated endpoints");
            }
        }
    }
    Ok(())
}

/// Resolve one Service port to the container port Endpoints must carry
/// (Sprint 18 / S2). Missing `targetPort` = identity; numeric (or numeric
/// string) = used verbatim; a name is looked up in the selected pods'
/// `spec.containers[].ports` (first pod wins, k8s-style). An unresolvable
/// name yields None -> the port is omitted from Endpoints (upstream drops
/// it too rather than guessing).
fn resolve_target_port(port_entry: &Value, pods: &[Value]) -> Option<u16> {
    let target = port_entry.get("targetPort");
    match target {
        None => port_entry.get("port")?.as_u64()?.try_into().ok(),
        Some(Value::Number(n)) => n.as_u64()?.try_into().ok(),
        Some(Value::String(s)) => {
            if let Ok(n) = s.parse::<u16>() {
                return Some(n);
            }
            for pod in pods {
                let Some(containers) = pod.pointer("/spec/containers").and_then(Value::as_array)
                else {
                    continue;
                };
                for c in containers {
                    let Some(ports) = c.get("ports").and_then(Value::as_array) else {
                        continue;
                    };
                    for cp in ports {
                        if cp.get("name").and_then(Value::as_str) == Some(s.as_str()) {
                            return cp.get("containerPort")?.as_u64()?.try_into().ok();
                        }
                    }
                }
            }
            None
        }
        Some(_) => None,
    }
}

/// True when the selector carries no constraints (selects nothing). Handles
/// both shapes: a plain label map (Service `spec.selector`) and a
/// LabelSelector (`matchLabels`/`matchExpressions`).
fn selector_is_empty(sel: &Value) -> bool {
    let is_selector_shape =
        sel.get("matchLabels").is_some() || sel.get("matchExpressions").is_some();
    if !is_selector_shape {
        return sel.as_object().is_none_or(|m| m.is_empty());
    }
    let has_labels = sel
        .get("matchLabels")
        .and_then(Value::as_object)
        .is_some_and(|m| !m.is_empty());
    let has_exprs = sel
        .get("matchExpressions")
        .and_then(Value::as_array)
        .is_some_and(|a| !a.is_empty());
    !(has_labels || has_exprs)
}

fn resource_version_of(v: &Value) -> Option<u64> {
    v.pointer("/metadata/resourceVersion")
        .and_then(Value::as_str)
        .and_then(|s| s.parse().ok())
}

#[cfg(test)]
mod tests {
    use super::resolve_target_port;
    use serde_json::{json, Value};

    fn pod_with_named_port() -> Value {
        json!({
            "metadata": {"name": "p", "namespace": "default"},
            "spec": {"containers": [{"name": "c", "image": "x", "ports": [
                {"name": "web", "containerPort": 9000},
            ]}]},
        })
    }

    #[test]
    fn resolve_target_port_identity_when_missing() {
        assert_eq!(resolve_target_port(&json!({"port": 80}), &[]), Some(80));
    }

    #[test]
    fn resolve_target_port_numeric() {
        assert_eq!(
            resolve_target_port(&json!({"port": 80, "targetPort": 8080}), &[]),
            Some(8080)
        );
    }

    #[test]
    fn resolve_target_port_numeric_string() {
        assert_eq!(
            resolve_target_port(&json!({"port": 80, "targetPort": "8080"}), &[]),
            Some(8080)
        );
    }

    #[test]
    fn resolve_target_port_named_from_pod() {
        let pods = [pod_with_named_port()];
        assert_eq!(
            resolve_target_port(&json!({"port": 80, "targetPort": "web"}), &pods),
            Some(9000)
        );
    }

    #[test]
    fn resolve_target_port_named_unmatched_is_none() {
        let pods = [pod_with_named_port()];
        assert_eq!(
            resolve_target_port(&json!({"port": 80, "targetPort": "nope"}), &pods),
            None
        );
    }

    #[test]
    fn resolve_target_port_missing_port_is_none() {
        assert_eq!(resolve_target_port(&json!({"name": "http"}), &[]), None);
    }
}
