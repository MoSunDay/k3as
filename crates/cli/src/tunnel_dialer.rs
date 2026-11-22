//! Node-side reverse relay tunnel dialer (TODO **T4.5**, decisions
//! **Q65/Q31/Q47**).
//!
//! The agent keeps ONE outbound long-poll parked on the apiserver; while it
//! is parked the node's tunnel is LIVE and the apiserver may hand single
//! kubelet API requests (logs/exec) back through it — the apiserver never
//! needs to dial the node (agent-dialed topology, Q65). The dialer executes
//! handed-off requests against the LOCAL kubelet listener
//! (`node_ip:kubelet_port`, the same address the kubelet binds, T4.2 Scope
//! C) by reusing [`apiserver::kubelet_hop::call_kubelet`], so the frozen
//! hop wire contract has exactly one implementation.
//!
//! Loop (HTTP/JSON only, Q10; single-request relay semantics, Q31 locked —
//! no WebSocket/SPDY/remotedialer):
//!   1. `POST /api/v1/nodes/{name}/tunnel/register` — heartbeat;
//!   2. `GET  /api/v1/nodes/{name}/tunnel/next` — long-poll (server parks
//!      ~25s; client cap 35s): `204` -> re-poll, `200` -> one
//!      [`apiserver::RelayRequest`];
//!   3. execute locally, then `POST .../tunnel/responses/{id}` with the
//!      [`apiserver::RelayResponse`].
//!
//! SLA (Q65): exponential backoff 1s -> 2s -> ... capped at 15s on any
//! transport failure (a dropped tunnel re-registers well inside the 30s
//! wall-clock bound); the apiserver bounds the relayed response wait at 10s
//! while the local hop itself is capped at 5s
//! ([`apiserver::kubelet_hop::HOP_TIMEOUT`]).
//!
//! Auth (Q47): the dialer presents the agent's static bearer token exactly
//! like every other agent->apiserver call; the kubelet itself stays bound
//! to the node IP (loopback/node-IP only).

use std::sync::Arc;
use std::time::Duration;

use apiserver::kubelet_hop::call_kubelet;
use apiserver::{RelayRequest, RelayResponse};
use infra::Shutdown;
use kubelet::HttpJson;

/// Client-side cap for one long-poll exchange: the apiserver parks at most
/// [`apiserver::LONG_POLL_TIMEOUT`] (25s); 35s tolerates scheduling slop.
const POLL_CLIENT_CAP: Duration = Duration::from_secs(35);
/// Backoff ladder start (Q65: 1s).
const BACKOFF_START: Duration = Duration::from_secs(1);
/// Backoff ladder cap (Q65: 15s — worst-case re-register stays far inside
/// the 30s dropped-tunnel SLA even with several failed attempts).
const BACKOFF_CAP: Duration = Duration::from_secs(15);

/// Next rung of the backoff ladder (pure; unit-tested).
fn next_backoff(current: Duration) -> Duration {
    std::cmp::min(current.saturating_mul(2), BACKOFF_CAP)
}

/// `POST /api/v1/nodes/{name}/tunnel/register` (pure; unit-tested).
fn register_path(node: &str) -> String {
    format!("/api/v1/nodes/{node}/tunnel/register")
}

/// `GET /api/v1/nodes/{name}/tunnel/next` (pure; unit-tested).
fn next_path(node: &str) -> String {
    format!("/api/v1/nodes/{node}/tunnel/next")
}

/// `POST /api/v1/nodes/{name}/tunnel/responses/{id}` (pure; unit-tested).
fn response_path(node: &str, id: &str) -> String {
    format!("/api/v1/nodes/{node}/tunnel/responses/{id}")
}

/// Decode one handed-off request's optional body (base64 -> bytes).
fn decode_body(request: &RelayRequest) -> Result<Option<Vec<u8>>, String> {
    match &request.body_b64 {
        None => Ok(None),
        Some(encoded) => common::b64::decode(encoded).map(Some),
    }
}

/// 502-shaped response carrying the failure text.
fn bad_gateway(message: &str) -> RelayResponse {
    let mut headers = std::collections::HashMap::new();
    headers.insert(
        "content-type".to_string(),
        "text/plain; charset=utf-8".to_string(),
    );
    RelayResponse {
        status: 502,
        headers,
        body_b64: Some(common::b64::encode(message.as_bytes())),
    }
}

/// POST one relayed response; failures only WARN (the apiserver answers
/// the submitter with a 404-equivalent timeout after its SLA).
async fn post_response(api: &HttpJson, node: &str, id: &str, response: RelayResponse) {
    let body = serde_json::to_value(&response).unwrap_or(serde_json::Value::Null);
    match api.post_json(&response_path(node, id), &body).await {
        Ok((code, _)) if (200..300).contains(&code) => {}
        Ok((code, _)) => tracing::warn!(
            target: "init-pro",
            code,
            "tunnel dialer: response post rejected"
        ),
        Err(e) => {
            tracing::warn!(target: "init-pro", error = %e, "tunnel dialer: response post failed")
        }
    }
}

/// One dialer instance bound to a node + local kubelet address.
struct Dialer {
    api: Arc<HttpJson>,
    node: String,
    kubelet_ip: String,
    kubelet_port: u16,
}

impl Dialer {
    /// Register/heartbeat; `Ok` only on a 2xx ack.
    async fn register(&self) -> Result<(), String> {
        match self
            .api
            .post_json(&register_path(&self.node), &serde_json::json!({}))
            .await
        {
            Ok((code, _)) if (200..300).contains(&code) => Ok(()),
            Ok((code, body)) => Err(format!("register answered {code}: {body}")),
            Err(e) => Err(e.to_string()),
        }
    }

    /// Long-poll once; `Ok(None)` = empty window (204), `Ok(Some)` = one
    /// handed-off request. Transport-level failures are `Err` (loop backs
    /// off and re-registers — a dropped tunnel re-registers <=30s, Q65).
    async fn poll_next(&self) -> Result<Option<RelayRequest>, String> {
        let (code, body) = self
            .api
            .request("GET", &next_path(&self.node), None)
            .await
            .map_err(|e| e.to_string())?;
        match code {
            204 => Ok(None),
            200 => serde_json::from_slice::<RelayRequest>(&body)
                .map(Some)
                .map_err(|e| format!("bad relay request payload: {e}")),
            other => Err(format!("tunnel/next answered {other}")),
        }
    }

    /// Execute + respond for one handed-off request (see [`execute_locally`]).
    async fn execute(&self, request: RelayRequest) {
        tracing::debug!(
            target: "init-pro",
            node = %self.node,
            id = %request.id,
            method = %request.method,
            path = %request.path,
            "tunnel dialer: executing relayed kubelet request locally"
        );
        let body = match decode_body(&request) {
            Ok(b) => b,
            Err(e) => {
                post_response(&self.api, &self.node, &request.id, bad_gateway(&e)).await;
                return;
            }
        };
        let content_type = request.headers.get("content-type").cloned();
        let reply = call_kubelet(
            &self.kubelet_ip,
            self.kubelet_port,
            &request.method,
            &request.path,
            content_type.as_deref(),
            body,
        )
        .await;
        let response = match reply {
            Ok(r) => RelayResponse::from_reply(&r),
            Err(e) => bad_gateway(&e),
        };
        post_response(&self.api, &self.node, &request.id, response).await;
    }

    /// The dialer loop: register -> park long-poll -> execute -> respond,
    /// backing off exponentially on any failure (Q65 SLA), until shutdown.
    async fn run(self, shutdown: Shutdown) {
        let mut backoff = BACKOFF_START;
        loop {
            if let Err(e) = self.register().await {
                tracing::warn!(target: "init-pro", error = %e, "tunnel dialer: register failed");
                if sleep_or_shutdown(backoff, &shutdown).await {
                    return;
                }
                backoff = next_backoff(backoff);
                continue;
            }
            let poll = tokio::select! {
                biased;
                () = shutdown.cancelled() => return,
                poll = self.poll_next() => poll,
            };
            match poll {
                // Empty window: tunnel healthy, immediately re-poll.
                Ok(None) => backoff = BACKOFF_START,
                // One relayed request: serve it, then re-poll.
                Ok(Some(request)) => {
                    backoff = BACKOFF_START;
                    self.execute(request).await;
                }
                Err(e) => {
                    tracing::warn!(target: "init-pro", error = %e, "tunnel dialer: poll failed");
                    if sleep_or_shutdown(backoff, &shutdown).await {
                        return;
                    }
                    backoff = next_backoff(backoff);
                }
            }
        }
    }
}

/// Sleep `dur` unless shutdown fires first; returns `true` when shutting down.
async fn sleep_or_shutdown(dur: Duration, shutdown: &Shutdown) -> bool {
    tokio::select! {
        biased;
        () = shutdown.cancelled() => true,
        _ = tokio::time::sleep(dur) => false,
    }
}

/// Spawn the dialer (T4.5): `None` when the api URL is unusable (the
/// kubelet stays on its degraded warn path — same posture as the other
/// keepers). The returned handle is drained by `run_supervised` after the
/// netpol keeper; the loop exits on the shared shutdown token between
/// exchanges (the parked poll is aborted by `select!`).
pub(crate) fn spawn_tunnel_dialer(
    api_url: &str,
    node_name: &str,
    kubelet_ip: &str,
    kubelet_port: u16,
    token: Option<&str>,
    shutdown: Shutdown,
) -> Option<tokio::task::JoinHandle<()>> {
    let api = match HttpJson::parse_url(api_url) {
        Ok(c) => Arc::new(
            c.with_token(token.map(str::to_string))
                // Q65: the parked long-poll emits no bytes for ~25s; the
                // 10s default per-read timeout would abort healthy polls.
                .with_read_timeout(POLL_CLIENT_CAP),
        ),
        Err(e) => {
            tracing::warn!(target: "init-pro", error = %e, "tunnel dialer: bad api url");
            return None;
        }
    };
    let dialer = Dialer {
        api,
        node: node_name.to_string(),
        kubelet_ip: kubelet_ip.to_string(),
        kubelet_port,
    };
    Some(tokio::spawn(async move {
        dialer.run(shutdown).await;
        tracing::info!(target: "init-pro", "tunnel dialer drained (T4.5)");
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_ladder_doubles_then_caps_at_15s() {
        assert_eq!(next_backoff(BACKOFF_START), Duration::from_secs(2));
        assert_eq!(next_backoff(Duration::from_secs(2)), Duration::from_secs(4));
        assert_eq!(
            next_backoff(Duration::from_secs(8)),
            Duration::from_secs(15)
        );
        assert_eq!(next_backoff(BACKOFF_CAP), BACKOFF_CAP);
    }

    #[test]
    fn tunnel_paths_match_the_http_surface() {
        assert_eq!(register_path("n1"), "/api/v1/nodes/n1/tunnel/register");
        assert_eq!(next_path("n1"), "/api/v1/nodes/n1/tunnel/next");
        assert_eq!(
            response_path("n1", "abc"),
            "/api/v1/nodes/n1/tunnel/responses/abc"
        );
    }

    #[test]
    fn bad_gateway_response_is_base64_text_plain() {
        let r = bad_gateway("connect failed");
        assert_eq!(r.status, 502);
        assert_eq!(r.headers["content-type"], "text/plain; charset=utf-8");
        let body = common::b64::decode(r.body_b64.as_deref().unwrap()).unwrap();
        assert_eq!(body, b"connect failed");
    }

    #[test]
    fn client_poll_cap_exceeds_the_server_window() {
        // Q65: the client must out-wait the apiserver's 25s park window.
        assert!(POLL_CLIENT_CAP > apiserver::LONG_POLL_TIMEOUT);
    }
}
