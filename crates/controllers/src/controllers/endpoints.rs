//! Endpoints controller (T3.1a).
//!
//! Reflects Service selector membership into an Endpoints object. Placeholder
//! IPs: v1 pods have no kubelet/CNI addresses yet, so each address is the
//! deterministic `10.42.x.y` derived from the pod identity (T4.2/T4.3 replace
//! this with real podIPs). Readiness: pods without ANY conditions count as
//! ready -- without kubelet nothing would ever report Ready and Endpoints
//! would stay empty.

use serde_json::{json, Value};
use storage::Key;

use crate::client::Client;
use crate::controllers::is_terminating;
use crate::error::ControllerError;
use crate::id::placeholder_pod_ip;
use crate::object::{name, namespace, selector_matches, semantic_eq};

/// Reconcile one Service's Endpoints. `endpoints` is the existing object, if
/// any (caller reads it fresh); the write happens only on change.
pub async fn reconcile(
    client: &std::sync::Arc<dyn Client>,
    service: &Value,
    endpoints: Option<&Value>,
) -> Result<(), ControllerError> {
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

    let ports: Vec<Value> = service
        .pointer("/spec/ports")
        .and_then(Value::as_array)
        .map(|ps| {
            ps.iter()
                .map(|p| {
                    let mut port = json!({
                        "port": p.get("port").cloned().unwrap_or(json!(0)),
                        "protocol": p.get("protocol").cloned().unwrap_or(json!("TCP")),
                    });
                    if let Some(n) = p.get("name") {
                        port["name"] = n.clone(); // targetPort is dropped
                    }
                    port
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

/// No conditions at all -> ready (v1 default, see module docs).
fn pod_is_ready(pod: &Value) -> bool {
    match pod.pointer("/status/conditions").and_then(Value::as_array) {
        None => true,
        Some(conditions) => conditions
            .iter()
            .find(|c| c.get("type").and_then(Value::as_str) == Some("Ready"))
            .map(|c| c.get("status").and_then(Value::as_str) == Some("True"))
            .unwrap_or(true),
    }
}

fn resource_version_of(v: &Value) -> Option<u64> {
    v.pointer("/metadata/resourceVersion")
        .and_then(Value::as_str)
        .and_then(|s| s.parse().ok())
}
