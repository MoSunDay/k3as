//! Reverse relay tunnel HTTP surface (TODO **T4.5**, decisions **Q65/Q31/Q47**):
//! the three agent-dialed routes the node's dialer (`crates/cli/src/tunnel_dialer.rs`)
//! speaks — plain HTTP + JSON only (Q10), single-request relay semantics
//! (Q31 locked: no WebSocket/SPDY/remotedialer multiplexing).
//!
//!   - `POST /api/v1/nodes/{name}/tunnel/register` — register/heartbeat the
//!     node's tunnel. Liveness itself is NOT set here: a tunnel is LIVE only
//!     while a long-poll is parked (see [`crate::tunnel_registry`]).
//!   - `GET  /api/v1/nodes/{name}/tunnel/next` — long-poll (server-side
//!     window [`crate::tunnel_registry::LONG_POLL_TIMEOUT`], ~25s): `204`
//!     when empty, `200` + one pending relayed request otherwise.
//!   - `POST /api/v1/nodes/{name}/tunnel/responses/{id}` — the agent posts
//!     the relayed response; unknown/expired ids answer `404`.
//!
//! Auth posture (Q47): the dialer presents the agent's static bearer token,
//! exactly like every other agent->apiserver call. Path shape parses as a
//! non-resource target under the RBAC subset, so any AUTHENTICATED identity
//! passes (the `/version` posture); the anonymous default plane stays
//! layer-free and byte-identical to pre-T4.5.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

use crate::error::status_failure;
use crate::state::AppState;
use crate::tunnel_registry::{LongPollGuard, LONG_POLL_TIMEOUT};
use crate::tunnel_wire::{RelayRequest, RelayResponse};

/// `POST /api/v1/nodes/{name}/tunnel/register` — heartbeat ack.
pub(crate) async fn register(State(st): State<AppState>, Path(name): Path<String>) -> Response {
    tracing::debug!(target: "init-pro", node = %name, "tunnel register/heartbeat");
    let _ = st.tunnels; // liveness is park-driven; nothing to mutate here.
    (
        StatusCode::OK,
        axum::Json(json!({"node": name, "registered": true})),
    )
        .into_response()
}

/// `GET /api/v1/nodes/{name}/tunnel/next` — the parked long-poll.
pub(crate) async fn next(State(st): State<AppState>, Path(name): Path<String>) -> Response {
    let park = st.tunnels.park(&name);
    let guard = LongPollGuard {
        tunnels: st.tunnels.clone(),
        node: name.clone(),
        generation: park.generation,
    };
    let request = match tokio::time::timeout(LONG_POLL_TIMEOUT, park.rx).await {
        // A request was handed off: this poll's seat is consumed and the
        // guard's unpark becomes a generation no-op.
        Ok(Ok(request)) => request,
        // Empty window (or the poll was replaced by a newer one): 204.
        _ => return StatusCode::NO_CONTENT.into_response(),
    };
    drop(guard);
    trace_relayed(&name, &request);
    (StatusCode::OK, axum::Json(relay_request_json(&request))).into_response()
}

/// `POST /api/v1/nodes/{name}/tunnel/responses/{id}` — resolve the waiter.
pub(crate) async fn respond(
    State(st): State<AppState>,
    Path((name, id)): Path<(String, String)>,
    axum::Json(response): axum::Json<RelayResponse>,
) -> Response {
    if st.tunnels.complete(&name, &id, response) {
        (StatusCode::OK, axum::Json(json!({"accepted": true}))).into_response()
    } else {
        status_failure(
            StatusCode::NOT_FOUND,
            "NotFound",
            format!("no pending tunnel relay {id} for node \"{name}\""),
        )
    }
}

/// Serialize with camelCase wire keys (serde rename on the wire types).
fn relay_request_json(request: &RelayRequest) -> serde_json::Value {
    serde_json::to_value(request).unwrap_or_else(|_| json!({}))
}

fn trace_relayed(node: &str, request: &RelayRequest) {
    tracing::debug!(
        target: "init-pro",
        node,
        id = %request.id,
        method = %request.method,
        path = %request.path,
        "tunnel relay submitted to agent"
    );
}
