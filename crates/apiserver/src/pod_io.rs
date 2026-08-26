//! Pod I/O relay subresources: `pods/log` + `pods/exec` (TODO **T4.2 Scope C**).
//!
//! The apiserver owns no containers: both subresources RESOLVE where a pod
//! runs (`spec.nodeName` -> Node `status.addresses[type=InternalIP]`, port
//! from the Node annotation `init-pro.io/kubelet-port`, fallback 10250) and
//! relay bytes to/from that kubelet's plain-HTTP listener:
//!   - `GET /api/v1/namespaces/<ns>/pods/<name>/log[?container=C][&tailLines=N]`
//!   - `POST /api/v1/namespaces/<ns>/pods/<name>/exec[?container=C]` `{"command":[..]}`
//!
//! The kubelet hop itself is byte-transparent (status + body + Content-Type
//! pass through verbatim; logs stay text/plain — decision **Q31**, everything
//! else JSON-only per **Q10**); its transport lives in [`crate::kubelet_hop`].

use std::collections::HashMap;

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::Value;

use crate::error::{status_failure, storage_error, ApiError};
use crate::kubelet_hop::{call_kubelet, relay_reply};
use crate::state::{cluster_key, namespaced_key, AppState};

/// Kubelet listener port annotation on the Node object (frozen contract).
const KUBELET_PORT_ANNOTATION: &str = "init-pro.io/kubelet-port";
/// Fallback kubelet port when the annotation is absent or invalid.
const DEFAULT_KUBELET_PORT: u16 = 10250;

// ---------------------------------------------------------------- pure parts

/// Read `metadata.uid`; missing/empty -> `None` (upstream always sets it,
/// but hand-written fixtures may not -> explicit 404, not a panic).
pub(crate) fn pod_uid(pod: &Value) -> Option<String> {
    pod.pointer("/metadata/uid")
        .and_then(Value::as_str)
        .filter(|uid| !uid.is_empty())
        .map(str::to_string)
}

/// First `status.addresses[]` entry of type `InternalIP`.
pub(crate) fn internal_ip(node: &Value) -> Option<String> {
    let addresses = node.pointer("/status/addresses")?.as_array()?;
    for address in addresses {
        if address.get("type").and_then(Value::as_str) == Some("InternalIP") {
            if let Some(ip) = address.get("address").and_then(Value::as_str) {
                return Some(ip.to_string());
            }
        }
    }
    None
}

/// Kubelet port from the node annotation; absent, non-numeric or zero all
/// fall back to [`DEFAULT_KUBELET_PORT`] (frozen contract: never fail here).
pub(crate) fn kubelet_port(node: &Value) -> u16 {
    // NOTE: `get`, not a JSON pointer — the annotation key contains '/'.
    node.get("metadata")
        .and_then(|m| m.get("annotations"))
        .and_then(|a| a.get(KUBELET_PORT_ANNOTATION))
        .and_then(Value::as_str)
        .and_then(|raw| raw.trim().parse::<u16>().ok())
        .filter(|port| *port != 0)
        .unwrap_or(DEFAULT_KUBELET_PORT)
}

/// Validate the optional `tailLines` query value (`""` = absent).
pub(crate) fn parse_tail_lines(raw: &str) -> Result<Option<u64>, String> {
    match raw {
        "" => Ok(None),
        v => v
            .parse::<u64>()
            .map(Some)
            .map_err(|_| format!("invalid tailLines {v:?}: must be a non-negative integer")),
    }
}

/// The exec body must parse as an object carrying a non-empty array of
/// strings under `command`; only then are the ORIGINAL bytes forwarded.
pub(crate) fn validate_exec_command(body: &[u8]) -> Result<(), String> {
    let parsed: Value =
        serde_json::from_slice(body).map_err(|e| format!("invalid JSON body: {e}"))?;
    if !parsed.is_object() {
        return Err("exec body must be a JSON object".into());
    }
    let command = parsed
        .get("command")
        .ok_or_else(|| "exec body must carry a \"command\" field".to_string())?;
    let items = command
        .as_array()
        .ok_or_else(|| "\"command\" must be an array of strings".to_string())?;
    if items.is_empty() || items.iter().any(|item| !item.is_string()) {
        return Err("\"command\" must be a non-empty array of strings".into());
    }
    Ok(())
}

/// Frozen-contract log target. `tailLines` appears ONLY when set; the
/// container `-` sentinel means "kubelet auto-picks".
pub(crate) fn log_target(uid: &str, container: &str, tail_lines: Option<u64>) -> String {
    match tail_lines {
        Some(n) => format!("/pods/{uid}/containers/{container}/log?tailLines={n}"),
        None => format!("/pods/{uid}/containers/{container}/log"),
    }
}

/// Frozen-contract exec target.
pub(crate) fn exec_target(uid: &str, container: &str) -> String {
    format!("/pods/{uid}/containers/{container}/exec")
}

// -------------------------------------------------------------- resolution

/// Where (and under which uid path prefix) to relay.
struct KubeletTarget {
    ip: String,
    port: u16,
    uid: String,
}

/// Resolution chain steps 1-4 of the frozen contract, shared by both verbs.
async fn resolve_kubelet(
    st: &AppState,
    namespace: &str,
    name: &str,
) -> Result<KubeletTarget, Response> {
    // Step 1: the pod must exist -> `pods "<name>" not found`.
    let entry = match st
        .store
        .get(&namespaced_key("", "pods", namespace, name))
        .await
    {
        Ok(Some(entry)) => entry,
        Ok(None) => {
            return Err(ApiError::NotFound {
                kind: "pods".into(),
                name: name.to_string(),
            }
            .into_response())
        }
        Err(e) => return Err(storage_error(e, "pods", name).into_response()),
    };

    // Step 2: unscheduled pods have nowhere to relay to.
    let node_name = entry
        .value
        .pointer("/spec/nodeName")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if node_name.is_empty() {
        return Err(status_failure(
            StatusCode::NOT_FOUND,
            "NotFound",
            "pod not yet scheduled to a node".into(),
        ));
    }

    // Step 3: the node object must exist.
    let node = match st.store.get(&cluster_key("", "nodes", node_name)).await {
        Ok(Some(node)) => node.value,
        Ok(None) => {
            return Err(ApiError::NotFound {
                kind: "nodes".into(),
                name: node_name.to_string(),
            }
            .into_response())
        }
        Err(e) => return Err(storage_error(e, "nodes", node_name).into_response()),
    };

    // Step 4a: no InternalIP means we cannot dial any kubelet.
    let Some(ip) = internal_ip(&node) else {
        return Err(status_failure(
            StatusCode::BAD_GATEWAY,
            "ServiceUnavailable",
            format!("node \"{node_name}\" has no InternalIP address"),
        ));
    };
    let Some(uid) = pod_uid(&entry.value) else {
        return Err(status_failure(
            StatusCode::NOT_FOUND,
            "NotFound",
            "pod has no uid".into(),
        ));
    };
    Ok(KubeletTarget {
        ip,
        port: kubelet_port(&node),
        uid,
    })
}

/// Consume the resolution chain, then run one hop; either way a Response.
async fn relay(
    st: &AppState,
    namespace: &str,
    name: &str,
    method: &str,
    request_target: impl Fn(&KubeletTarget) -> String,
    content_type: Option<&str>,
    body: Option<Vec<u8>>,
) -> Response {
    let target = match resolve_kubelet(st, namespace, name).await {
        Ok(target) => target,
        Err(response) => return response,
    };
    relay_reply(
        call_kubelet(
            &target.ip,
            target.port,
            method,
            &request_target(&target),
            content_type,
            body,
        )
        .await,
    )
    .await
}

// ------------------------------------------------------------------- routes

/// `GET /api/v1/namespaces/{namespace}/pods/{name}/log`
/// (?container=C&tailLines=N — both optional; container `-` = auto-pick).
pub(crate) async fn pod_log(
    State(st): State<AppState>,
    Path((namespace, name)): Path<(String, String)>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    // Query validation precedes storage access (cheap 400s first).
    let tail = match parse_tail_lines(q.get("tailLines").map(String::as_str).unwrap_or("")) {
        Ok(tail) => tail,
        Err(msg) => return status_failure(StatusCode::BAD_REQUEST, "BadRequest", msg),
    };
    let container = q
        .get("container")
        .map(String::as_str)
        .unwrap_or("-")
        .to_string();
    relay(
        &st,
        &namespace,
        &name,
        "GET",
        |t| log_target(&t.uid, &container, tail),
        None,
        None,
    )
    .await
}

/// `POST /api/v1/namespaces/{namespace}/pods/{name}/exec` (?container=C):
/// a validated `{"command":[..]}` body is forwarded VERBATIM.
pub(crate) async fn pod_exec(
    State(st): State<AppState>,
    Path((namespace, name)): Path<(String, String)>,
    Query(q): Query<HashMap<String, String>>,
    body: Bytes,
) -> Response {
    // T4.2 Scope C: validate FIRST, else 400 before any storage access.
    if let Err(msg) = validate_exec_command(&body) {
        return status_failure(StatusCode::BAD_REQUEST, "BadRequest", msg);
    }
    let container = q
        .get("container")
        .map(String::as_str)
        .unwrap_or("-")
        .to_string();
    relay(
        &st,
        &namespace,
        &name,
        "POST",
        |t| exec_target(&t.uid, &container),
        Some("application/json"),
        Some(body.to_vec()),
    )
    .await
}
