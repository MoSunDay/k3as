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
//! layer-free and byte-identical to pre-T4.5. On top of that, S53 / Q65
//! (review P1-③) CLOSED the "any authenticated identity can hijack another
//! node's park seat or inject forged responses" hole with park identity
//! binding: `register` hands out a rotated `parkToken` (single active
//! value), and `next`/`responses` must present it as the `parkToken` query
//! parameter — the apiserver validates ownership inside the SAME critical
//! section that seats the park / resolves the pending responder (no
//! TOCTOU). A missing OR mismatched token is 401 fail-closed with a
//! generic message (no node-existence oracle); auth runs BEFORE the
//! pending-id lookup in `respond`. Version skew: an old agent against a
//! new server gets 401 forever — the dialer backs off and re-registers
//! (fresh token, self-heal); the direct-first Q65 posture is unchanged.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::json;

use crate::error::status_failure;
use crate::state::AppState;
use crate::tunnel_registry::{LongPollGuard, LONG_POLL_TIMEOUT};
use crate::tunnel_wire::{RegisterAck, RelayRequest, RelayResponse};

/// S53 / Q65 (review P1-③) credential as `?parkToken=` (absent parses to
/// `None` -> 401 fail-closed: a stripped parameter is indistinguishable
/// from an old client, so there is NO absent-tolerance). `pub(crate)`
/// because the handler signature leaks it into axum's `Handler` bounds
/// (checked in `app.rs`).
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ParkQuery {
    park_token: Option<String>,
}

/// Generic 401 for every failed ownership check: identical bytes for a
/// missing and a mismatched credential (no node-existence oracle).
fn park_token_unauthorized() -> Response {
    status_failure(
        StatusCode::UNAUTHORIZED,
        "Unauthorized",
        "invalid or missing parkToken (tunnel park identity, Q65 S53 P1-③)".to_string(),
    )
}

/// `POST /api/v1/nodes/{name}/tunnel/register` — heartbeat ack.
pub(crate) async fn register(State(st): State<AppState>, Path(name): Path<String>) -> Response {
    tracing::debug!(target: "init-pro", node = %name, "tunnel register/heartbeat");
    // S53 / Q65 (review P1-③): rotate the park credential and hand it out
    // in the ack (single active value; the previous token dies now).
    // Liveness is NOT set here — a tunnel is LIVE only while a long-poll
    // is parked (see [`crate::tunnel_registry`]).
    let token = st.tunnels.register(&name);
    (
        StatusCode::OK,
        axum::Json(RegisterAck {
            node: name,
            registered: true,
            park_token: Some(token),
        }),
    )
        .into_response()
}

/// `GET /api/v1/nodes/{name}/tunnel/next` — the parked long-poll.
pub(crate) async fn next(
    State(st): State<AppState>,
    Path(name): Path<String>,
    Query(q): Query<ParkQuery>,
) -> Response {
    // S53 / Q65 (review P1-③): ownership check INSIDE the same critical
    // section that seats the park (no TOCTOU), fail-closed on any miss.
    let park = match st.tunnels.park_checked(&name, q.park_token.as_deref()) {
        Ok(park) => park,
        Err(_) => return park_token_unauthorized(),
    };
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
    Query(q): Query<ParkQuery>,
    axum::Json(response): axum::Json<RelayResponse>,
) -> Response {
    // S53 / Q65 (review P1-③): auth runs FIRST — before the pending-id
    // lookup, so an unauthenticated caller learns nothing about ids.
    match st
        .tunnels
        .complete_checked(&name, &id, q.park_token.as_deref(), response)
    {
        Ok(true) => (StatusCode::OK, axum::Json(json!({"accepted": true}))).into_response(),
        // Auth passed but the waiter already gave up: today's 404 shape.
        Ok(false) => status_failure(
            StatusCode::NOT_FOUND,
            "NotFound",
            format!("no pending tunnel relay {id} for node \"{name}\""),
        ),
        Err(_) => park_token_unauthorized(),
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
