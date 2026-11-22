//! Reverse relay tunnel registry (TODO **T4.5**, decisions **Q65/Q31/Q47**).
//!
//! Pure state machine (async map keyed by node name) behind the
//! agent-dialed reverse tunnel: the node's agent parks ONE long-poll on
//! the apiserver; while that poll is parked the node's tunnel is LIVE and
//! [`crate::kubelet_hop`] may submit single kubelet API requests through
//! it. HTTP/JSON only (Q10) — no WebSocket/SPDY/remotedialer multiplexing
//! (Q31 stays locked: one request per relayed exchange, single-response
//! semantics, existing clients unchanged).
//!
//! Semantics (unit-tested as pure state transitions):
//!  - `park` seats a long-poll and REPLACES any older one (the older
//!    receiver resolves with a disconnect error -> its handler answers 204
//!    and the dialer immediately re-polls);
//!  - liveness == "a long-poll is currently parked". No lease, no timer:
//!    the HTTP handler's 25s long-poll timeout unparks via the generation
//!    guard, and a dead client self-heals because the first submit on the
//!    stale sender fails and clears it;
//!  - `submit` hands one request to the parked poll and awaits the agent's
//!    response bounded by [`RELAY_TIMEOUT_SLA`] (10s);
//!  - `complete` resolves the pending responder by request id.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::oneshot;

use crate::kubelet_hop::KubeletReply;
use crate::tunnel_wire::{RelayRequest, RelayResponse};

/// Server-side long-poll window: a parked `tunnel/next` answers 204 after
/// this even when no relayed request arrived (keeps the dialer loop fresh).
pub const LONG_POLL_TIMEOUT: Duration = Duration::from_secs(25);
/// Apiserver-side SLA waiting for the agent's relayed response.
pub(crate) const RELAY_TIMEOUT_SLA: Duration = Duration::from_secs(10);

// ------------------------------------------------------- state machine -----

#[derive(Default)]
struct NodeTunnel {
    generation: u64,
    parked: Option<oneshot::Sender<RelayRequest>>,
    pending: HashMap<String, oneshot::Sender<RelayResponse>>,
}

/// A seated long-poll: await the receiver for the handed-off request;
/// `generation` guards a later [`TunnelRegistry::unpark`] against racing
/// a newer park.
pub(crate) struct Park {
    pub generation: u64,
    pub rx: oneshot::Receiver<RelayRequest>,
}

/// RAII seat for one parked long-poll: when the HTTP handler ends (request
/// handed off, 204 timeout, or the client vanished and axum dropped the
/// future) the seat is cleared via the generation guard. After a handoff
/// the seat is already consumed, so the drop is a no-op.
pub(crate) struct LongPollGuard {
    pub tunnels: Arc<TunnelRegistry>,
    pub node: String,
    pub generation: u64,
}

impl Drop for LongPollGuard {
    fn drop(&mut self) {
        self.tunnels.unpark(&self.node, self.generation);
    }
}

/// Shared registry threaded through [`crate::state::AppState`].
pub(crate) struct TunnelRegistry {
    relay_timeout: Duration,
    nodes: Mutex<HashMap<String, NodeTunnel>>,
}

impl TunnelRegistry {
    /// Production registry with the 10s relay SLA.
    pub(crate) fn new_shared() -> Arc<Self> {
        Arc::new(Self {
            relay_timeout: RELAY_TIMEOUT_SLA,
            nodes: Mutex::new(HashMap::new()),
        })
    }

    /// Test constructor with a shortened relay timeout.
    #[cfg(test)]
    pub(crate) fn with_relay_timeout(relay_timeout: Duration) -> Arc<Self> {
        Arc::new(Self {
            relay_timeout,
            nodes: Mutex::new(HashMap::new()),
        })
    }

    /// Seat this node's long-poll, replacing any older one.
    pub(crate) fn park(&self, node: &str) -> Park {
        let (tx, rx) = oneshot::channel();
        let mut nodes = self.nodes.lock().expect("tunnel registry poisoned");
        let entry = nodes.entry(node.to_string()).or_default();
        entry.generation += 1;
        entry.parked = Some(tx); // dropping the prior sender disconnects it
        Park {
            generation: entry.generation,
            rx,
        }
    }

    /// A long-poll ended without a request: clear the seat only when it is
    /// still THIS poll's generation (a newer park wins).
    pub(crate) fn unpark(&self, node: &str, generation: u64) {
        let mut nodes = self.nodes.lock().expect("tunnel registry poisoned");
        if let Some(entry) = nodes.get_mut(node) {
            if entry.generation == generation && entry.parked.is_some() {
                entry.parked = None;
            }
        }
    }

    /// LIVE while a long-poll is parked (Q65 liveness definition).
    pub(crate) fn is_live(&self, node: &str) -> bool {
        self.nodes
            .lock()
            .expect("tunnel registry poisoned")
            .get(node)
            .is_some_and(|entry| entry.parked.is_some())
    }

    /// Submit one kubelet request through the node's tunnel and await the
    /// agent's response (bounded by the relay SLA). Fails fast when the
    /// tunnel is not live — the caller keeps today's direct error (Q31).
    pub(crate) async fn submit(
        &self,
        node: &str,
        method: &str,
        path: &str,
        content_type: Option<&str>,
        body: Option<&[u8]>,
    ) -> Result<KubeletReply, String> {
        let mut headers = HashMap::new();
        if let Some(ct) = content_type {
            headers.insert("content-type".to_string(), ct.to_string());
        }
        let request = RelayRequest {
            id: new_request_id(),
            method: method.to_string(),
            path: path.to_string(),
            headers,
            body_b64: body.map(common::b64::encode),
        };
        // Hand off + register the responder under one short critical
        // section (no await while the mutex is held).
        let (responder, request_id) = {
            let mut nodes = self.nodes.lock().expect("tunnel registry poisoned");
            let entry = nodes
                .get_mut(node)
                .ok_or_else(|| format!("node \"{node}\" has no tunnel"))?;
            let sender = entry
                .parked
                .take()
                .ok_or_else(|| format!("node \"{node}\" tunnel not live"))?;
            let (tx, rx) = oneshot::channel();
            entry.pending.insert(request.id.clone(), tx);
            let request_id = request.id.clone();
            sender
                .send(request)
                .map_err(|_| format!("node \"{node}\" tunnel handoff failed (agent gone)"))?;
            (rx, request_id)
        };
        let outcome = tokio::time::timeout(self.relay_timeout, responder).await;
        match outcome {
            Ok(Ok(response)) => response.into_reply(),
            Ok(Err(_gone)) => Err(format!(
                "node \"{node}\" dropped the relayed request before answering"
            )),
            Err(_) => {
                self.drop_pending(node, &request_id);
                Err(format!(
                    "node \"{node}\" tunnel response timed out after {}s",
                    self.relay_timeout.as_secs()
                ))
            }
        }
    }

    /// The agent posted its relayed response: resolve the waiter by id.
    /// `false` = unknown/expired id (the waiter already gave up).
    pub(crate) fn complete(&self, node: &str, id: &str, response: RelayResponse) -> bool {
        let mut nodes = self.nodes.lock().expect("tunnel registry poisoned");
        match nodes
            .get_mut(node)
            .and_then(|entry| entry.pending.remove(id))
        {
            Some(sender) => sender.send(response).is_ok(),
            None => false,
        }
    }

    fn drop_pending(&self, node: &str, id: &str) {
        self.nodes
            .lock()
            .expect("tunnel registry poisoned")
            .get_mut(node)
            .map(|entry| entry.pending.remove(id));
    }
}

/// Random relay request id (uuid-v4 shape, `getrandom` like `state.rs`).
fn new_request_id() -> String {
    use std::fmt::Write as _;
    let mut b = [0u8; 16];
    getrandom::fill(&mut b).expect("system randomness unavailable");
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let mut out = String::with_capacity(36);
    for (i, x) in b.iter().enumerate() {
        if matches!(i, 4 | 6 | 8 | 10) {
            out.push('-');
        }
        let _ = write!(out, "{x:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kubelet_hop::KubeletReply;

    fn reply(code: u16, body: &str) -> KubeletReply {
        KubeletReply {
            code,
            content_type: Some("text/plain".to_string()),
            body: body.as_bytes().to_vec(),
        }
    }

    #[tokio::test]
    async fn not_live_without_parked_poll() {
        let reg = TunnelRegistry::new_shared();
        assert!(!reg.is_live("n1"));
        let err = reg
            .submit("n1", "GET", "/pods/u/containers/c/log", None, None)
            .await
            .unwrap_err();
        assert!(
            err.contains("not live") || err.contains("no tunnel"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn handoff_roundtrip() {
        let reg = TunnelRegistry::new_shared();
        let park = reg.park("n1");
        assert!(reg.is_live("n1"));

        // Fake agent: hold the long-poll, answer the handed-off request.
        let reg_agent = reg.clone();
        let agent = tokio::spawn(async move {
            let request = park.rx.await.expect("parked poll receives request");
            assert_eq!(request.method, "POST");
            assert_eq!(request.path, "/pods/uid/containers/c/exec");
            assert_eq!(
                request.headers.get("content-type").map(String::as_str),
                Some("application/json")
            );
            assert!(reg_agent.complete(
                "n1",
                &request.id,
                RelayResponse::from_reply(&reply(200, "ok"))
            ));
        });

        let got = reg
            .submit(
                "n1",
                "POST",
                "/pods/uid/containers/c/exec",
                Some("application/json"),
                Some(br#"{"command":["x"]}"#),
            )
            .await
            .expect("relayed reply");
        assert_eq!(got.code, 200);
        assert_eq!(got.body, b"ok");
        assert_eq!(got.content_type.as_deref(), Some("text/plain"));
        agent.await.unwrap();
    }

    #[tokio::test]
    async fn relay_timeout_cleans_pending() {
        let reg = TunnelRegistry::with_relay_timeout(Duration::from_millis(50));
        let park = reg.park("n1");
        // Agent parks but never answers.
        let err = reg
            .submit("n1", "GET", "/pods/u/containers/c/log", None, None)
            .await
            .unwrap_err();
        assert!(err.contains("timed out"), "{err}");
        // The stale pending entry must be gone; the parked poll still holds
        // the request, so completing late is a no-op (waiter left).
        drop(park);
        assert!(!reg.complete(
            "n1",
            "bogus",
            RelayResponse::from_reply(&reply(200, "late"))
        ));
    }

    #[tokio::test]
    async fn unpark_generation_guard() {
        let reg = TunnelRegistry::new_shared();
        let old = reg.park("n1");
        let new = reg.park("n1"); // replaces the older poll
        reg.unpark("n1", old.generation); // stale end: must not clear `new`
        assert!(reg.is_live("n1"));
        reg.unpark("n1", new.generation);
        assert!(!reg.is_live("n1"));
    }

    #[tokio::test]
    async fn dead_park_self_heals() {
        let reg = TunnelRegistry::new_shared();
        let park = reg.park("n1");
        drop(park.rx); // client vanished mid-poll
        assert!(reg.is_live("n1"), "liveness is optimistic until send");
        let err = reg
            .submit("n1", "GET", "/pods/u/containers/c/log", None, None)
            .await
            .unwrap_err();
        assert!(err.contains("handoff failed"), "{err}");
        // The stale seat was consumed: not live, and a fresh park works.
        assert!(!reg.is_live("n1"));
        let fresh = reg.park("n1");
        assert!(reg.is_live("n1"));
        drop(fresh);
    }
}
