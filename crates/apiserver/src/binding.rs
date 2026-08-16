//! Pod binding subresource (TODO **T3.2**, slice S2).
//!
//! `POST /api/v1/namespaces/<ns>/pods/<name>/binding` — the upstream bind
//! path kube-scheduler drives (`pods/binding`). The body is a core/v1
//! `Binding` whose `target` names a Node; a successful bind writes the pod's
//! `spec.nodeName` **in-process** through the store (decision **Q19**) and
//! echoes the Binding back with `201 Created`.
//!
//! Semantics (upstream parity):
//!  - pod missing -> `404 pods "<name>" not found`
//!  - pod already carries `spec.nodeName` -> `409 Conflict` ("pod ... is
//!    already assigned to node ..."); node existence is NOT checked here —
//!    the scheduler filters nodes itself before binding.
//!  - concurrent writer between the read and the CAS -> `409 Conflict`.

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

use crate::error::{storage_error, ApiError};
use crate::state::{namespaced_key, set_namespace, set_type_meta, AppState};

/// Extract the target node name from a Binding body.
///
/// Accepts the upstream shape `{"target": {"name": "n1"}}` and tolerates a
/// flattened `{"spec": {"nodeName": "n1"}}`.
pub(crate) fn target_node(body: &Value) -> Option<String> {
    body.pointer("/target/name")
        .and_then(|v| v.as_str())
        .or_else(|| body.pointer("/spec/nodeName").and_then(|v| v.as_str()))
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Shared binding logic (also the seam the in-process scheduler mirrors).
pub(crate) async fn do_bind(
    st: &AppState,
    namespace: &str,
    pod_name: &str,
    body: &Value,
) -> Response {
    let key = namespaced_key("", "pods", namespace, pod_name);
    let Some(node) = target_node(body) else {
        return ApiError::Invalid {
            kind: "Binding".into(),
            message: "Binding.target must name a node (target.name)".into(),
        }
        .into_response();
    };

    let (mut pod, mod_rev) = match st.store.get(&key).await {
        Ok(Some(e)) => (e.value, e.mod_revision),
        Ok(None) => {
            return ApiError::NotFound {
                kind: "pods".into(),
                name: pod_name.into(),
            }
            .into_response()
        }
        Err(e) => return storage_error(e, "pods", pod_name).into_response(),
    };

    if let Some(existing) = pod.pointer("/spec/nodeName").and_then(|v| v.as_str()) {
        if !existing.is_empty() {
            // Upstream message shape: "Operation cannot be fulfilled on pods/binding ...".
            let status = json!({
                "kind": "Status", "apiVersion": "v1", "metadata": {},
                "status": "Failure",
                "message": format!(
                    "Operation cannot be fulfilled on pods/binding \"{pod_name}\": pod \"{pod_name}\" is already assigned to node \"{existing}\""
                ),
                "reason": "Conflict",
                "code": 409,
            });
            return (StatusCode::CONFLICT, Json(status)).into_response();
        }
    }

    // Write spec.nodeName, preserving everything else, CAS on the revision we
    // just read (the entry's mod_revision, NOT the embedded
    // metadata.resourceVersion, which lags after any client read-modify-write
    // and would 409 forever) so a concurrent binder wins the race visibly.
    if let Some(spec) = pod.get_mut("spec").and_then(|s| s.as_object_mut()) {
        spec.insert("nodeName".into(), Value::String(node));
    } else {
        pod["spec"] = json!({ "nodeName": node });
    }
    match st.store.update(&key, pod, Some(mod_rev)).await {
        Ok(entry) => {
            // Echo the Binding (upstream returns the stored Binding, 201).
            let mut binding = body.clone();
            binding["metadata"]["name"] = json!(pod_name);
            set_type_meta(&mut binding, "v1", "Binding");
            set_namespace(&mut binding, namespace);
            if let Some(m) = binding.get_mut("metadata").and_then(|m| m.as_object_mut()) {
                m.insert(
                    "resourceVersion".to_string(),
                    Value::String(entry.mod_revision.to_string()),
                );
            }
            (StatusCode::CREATED, Json(binding)).into_response()
        }
        Err(e) => storage_error(e, "pods", pod_name).into_response(),
    }
}

/// `POST /api/v1/namespaces/{namespace}/pods/{name}/binding`
pub(crate) async fn bind_pod(
    State(st): State<AppState>,
    Path((namespace, name)): Path<(String, String)>,
    body: Bytes,
) -> Response {
    let parsed = match serde_json::from_slice::<Value>(&body) {
        Ok(v) => v,
        Err(_) => return ApiError::BadRequest("invalid JSON".into()).into_response(),
    };
    do_bind(&st, &namespace, &name, &parsed).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn target_node_accepts_upstream_and_flattened_shapes() {
        assert_eq!(
            target_node(&json!({"target": {"name": "n1"}})),
            Some("n1".into())
        );
        assert_eq!(
            target_node(&json!({"spec": {"nodeName": "n2"}})),
            Some("n2".into())
        );
        assert_eq!(target_node(&json!({"target": {"name": ""}})), None);
        assert_eq!(target_node(&json!({})), None);
    }
}
