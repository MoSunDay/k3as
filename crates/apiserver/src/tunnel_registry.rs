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
//!  - `register` (S53 / Q65 review P1-③) mints/rotates the node's park
//!    credential (`parkToken`, getrandom 128-bit hex): single active
//!    value, the previous token dies IMMEDIATELY, and generation/parked
//!    are never touched here (liveness stays park-driven);
//!  - `park_checked`/`complete_checked` (P1-③) validate the presented
//!    credential INSIDE the same critical section that seats the park /
//!    resolves the pending responder (no TOCTOU between check and
//!    effect); a missing or mismatched credential is an
//!    [`TunnelAuthError::InvalidParkToken`] -> the HTTP handler answers
//!    401 fail-closed (the two cases are deliberately indistinguishable);
//!  - `park_checked` seats a long-poll and REPLACES any older one (the older
//!    receiver resolves with a disconnect error -> its handler answers 204
//!    and the dialer immediately re-polls);
//!  - liveness == "a long-poll is currently parked". No lease, no timer:
//!    the HTTP handler's 25s long-poll timeout unparks via the generation
//!    guard, and a dead client self-heals because the first submit on the
//!    stale sender fails and clears it;
//!  - `submit` hands one request to the parked poll and awaits the agent's
//!    response bounded by [`RELAY_TIMEOUT_SLA`] (10s);
//!  - `complete_checked` resolves the pending responder by request id.

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
    /// S53 / Q65 (review P1-③): the node's active park credential
    /// (rotated by every `register`). The Default empty string means
    /// "never registered", so every checked call on a missing entry
    /// fails (401 fail-closed).
    park_token: String,
}

/// S53 / Q65 (review P1-③): park identity check failed. Missing and
/// mismatched credentials are DELIBERATELY indistinguishable — a stripped
/// query parameter behaves exactly like an old/wrong token (fail-closed;
/// no absent-tolerance, a stripped param is indistinguishable from an old
/// client). Maps to a 401 in the HTTP layer.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum TunnelAuthError {
    InvalidParkToken,
}

/// S53 / Q65 (review P1-③): the shared ownership predicate — fail when
/// the node never registered (empty stored token) or the presented
/// credential is missing/old/wrong. Pure; unit-tested via the checked
/// entry points.
fn park_token_mismatch(entry: &NodeTunnel, token: Option<&str>) -> bool {
    entry.park_token.is_empty() || token != Some(entry.park_token.as_str())
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

/// S53 / Q65 (review P1-②): cancellation-clean pending entry. `submit`
/// parks its responder in `pending` keyed by request id BEFORE awaiting
/// the agent; if the submit future itself is dropped first (HTTP caller
/// disconnected, axum dropped the handler), no outcome branch runs and
/// the entry would leak until a response that may never arrive. This
/// pure RAII guard removes the entry on Drop; every resolved outcome
/// disarms it first.
struct PendingGuard {
    tunnels: Arc<TunnelRegistry>,
    node: String,
    request_id: String,
    armed: bool,
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        if self.armed {
            self.tunnels.drop_pending(&self.node, &self.request_id);
        }
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

    /// S53 / Q65 (review P1-③): register/heartbeat the node's tunnel and
    /// ROTATE its park credential. Returns the freshly minted token — the
    /// single active value; the previous one is invalid immediately.
    /// MUST NOT touch generation/parked: liveness stays park-driven (a
    /// register replacing a live long-poll would break the Q65 liveness
    /// contract — re-parking is the dialer's job, right after the ack).
    pub(crate) fn register(&self, node: &str) -> String {
        let mut nodes = self.nodes.lock().expect("tunnel registry poisoned");
        let entry = nodes.entry(node.to_string()).or_default();
        entry.park_token = new_park_token();
        entry.park_token.clone()
    }

    /// Seat this node's long-poll, replacing any older one — but only
    /// after the S53 / Q65 (review P1-③) ownership check, run INSIDE the
    /// same critical section that seats the park (no TOCTOU: lock ->
    /// check token -> seat, all under the one lock). A missing node entry
    /// fails too (the Default entry carries an empty token).
    ///
    /// A8: the lookup must not insert. `entry().or_default()` ran BEFORE the
    /// token check, so an unauthenticated
    /// `GET .../tunnel/next?parkToken=x` with a random node name grew the
    /// map forever (one `NodeTunnel` per distinct attacker-chosen name).
    /// Unknown node -> the same `InvalidParkToken` error, no entry.
    pub(crate) fn park_checked(
        &self,
        node: &str,
        token: Option<&str>,
    ) -> Result<Park, TunnelAuthError> {
        let (tx, rx) = oneshot::channel();
        let mut nodes = self.nodes.lock().expect("tunnel registry poisoned");
        let entry = nodes
            .get_mut(node)
            .ok_or(TunnelAuthError::InvalidParkToken)?;
        if park_token_mismatch(entry, token) {
            return Err(TunnelAuthError::InvalidParkToken);
        }
        entry.generation += 1;
        entry.parked = Some(tx); // dropping the prior sender disconnects it
        Ok(Park {
            generation: entry.generation,
            rx,
        })
    }

    /// A8: drop a node's tunnel state (token, generation and any parked
    /// poll). Called when the apiserver observes the Node go away so a
    /// decommissioned node cannot be re-parked with a stale token and its
    /// entry does not linger for the life of the process.
    pub(crate) fn forget(&self, node: &str) {
        let mut nodes = self.nodes.lock().expect("tunnel registry poisoned");
        nodes.remove(node);
    }

    /// A8: number of tracked nodes (tests + leak introspection).
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.nodes.lock().expect("tunnel registry poisoned").len()
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
        self: &Arc<Self>,
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
        // S53 / Q65 (P1-②): arm the cancellation guard — if THIS future is
        // dropped before any outcome below runs, its Drop removes the
        // pending responder (same lock pattern as the timeout branch).
        let mut guard = PendingGuard {
            tunnels: Arc::clone(self),
            node: node.to_string(),
            request_id: request_id.clone(),
            armed: true,
        };
        let outcome = tokio::time::timeout(self.relay_timeout, responder).await;
        match outcome {
            Ok(Ok(response)) => {
                // `complete` already removed the entry before resolving.
                guard.armed = false;
                response.into_reply()
            }
            Ok(Err(_gone)) => {
                // Sender gone => the entry was already removed to drop it.
                guard.armed = false;
                Err(format!(
                    "node \"{node}\" dropped the relayed request before answering"
                ))
            }
            Err(_) => {
                self.drop_pending(node, &request_id);
                guard.armed = false;
                Err(format!(
                    "node \"{node}\" tunnel response timed out after {}s",
                    self.relay_timeout.as_secs()
                ))
            }
        }
    }

    /// The agent posted its relayed response: resolve the waiter by id.
    /// S53 / Q65 (review P1-③): the ownership check runs FIRST, inside
    /// the same critical section as the pending-remove + send (no TOCTOU;
    /// auth also precedes the id lookup, so an unauthenticated caller
    /// learns nothing about pending ids). `Ok(false)` = unknown/expired
    /// id after auth passed (the waiter already gave up — today's 404).
    pub(crate) fn complete_checked(
        &self,
        node: &str,
        id: &str,
        token: Option<&str>,
        response: RelayResponse,
    ) -> Result<bool, TunnelAuthError> {
        let mut nodes = self.nodes.lock().expect("tunnel registry poisoned");
        let entry = nodes
            .get_mut(node)
            .ok_or(TunnelAuthError::InvalidParkToken)?;
        if park_token_mismatch(entry, token) {
            return Err(TunnelAuthError::InvalidParkToken);
        }
        Ok(match entry.pending.remove(id) {
            Some(sender) => sender.send(response).is_ok(),
            None => false,
        })
    }

    /// S53 / Q65 (review P1-③) TEST-ONLY shim for the pre-P1-③ `park`
    /// surface: kept behind `#[cfg(test)]` for the older test suites that
    /// do not exercise credentials (kubelet_hop's `relay_tests`), so the
    /// checked entry points stay the ONLY production surface. The shim
    /// self-registers and presents the fresh token.
    #[cfg(test)]
    pub(crate) fn park(&self, node: &str) -> Park {
        let token = self.register(node);
        self.park_checked(node, Some(&token))
            .expect("test shim park")
    }

    /// S53 / Q65 (review P1-③) TEST-ONLY shim for the pre-P1-③ `complete`
    /// surface: presents the node's CURRENT token (never registers — a
    /// rotation here would invalidate the park the caller just seated).
    /// `Err` maps to `false` (today's "unknown id" answer).
    #[cfg(test)]
    pub(crate) fn complete(&self, node: &str, id: &str, response: RelayResponse) -> bool {
        let token = self
            .nodes
            .lock()
            .expect("tunnel registry poisoned")
            .get(node)
            .map(|entry| entry.park_token.clone())
            .unwrap_or_default();
        self.complete_checked(node, id, Some(token.as_str()), response)
            .unwrap_or_default()
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

/// S53 / Q65 (review P1-③): random 128-bit park credential — 16 getrandom
/// bytes rendered as 32 lowercase hex chars (mirrors [`new_request_id`]'s
/// style; plain hex so the query parameter needs no percent-encoding).
fn new_park_token() -> String {
    use std::fmt::Write as _;
    let mut b = [0u8; 16];
    getrandom::fill(&mut b).expect("system randomness unavailable");
    let mut out = String::with_capacity(32);
    for x in b.iter() {
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

    /// A8 regression: `park_checked` used to `or_default()` the node entry
    /// BEFORE the token check, so an unauthenticated park with a random node
    /// name grew the map forever. An unknown node must leave no trace.
    #[test]
    fn park_checked_with_unknown_node_leaves_no_entry() {
        let reg = TunnelRegistry::new_shared();
        for name in ["ghost-1", "ghost-2", "../../etc", ""] {
            assert!(
                matches!(
                    reg.park_checked(name, Some("x")),
                    Err(TunnelAuthError::InvalidParkToken)
                ),
                "an unknown node must fail closed"
            );
        }
        assert_eq!(
            reg.len(),
            0,
            "no node entry may be created by a failed park"
        );
        assert!(!reg.is_live("ghost-1"));
    }

    /// A8: a registered node whose token does not match is rejected the same
    /// way, and its (single) entry is neither duplicated nor re-seated.
    #[test]
    fn park_checked_with_a_wrong_token_does_not_seat_or_grow() {
        let reg = TunnelRegistry::new_shared();
        let tok = reg.register("n1");
        assert!(matches!(
            reg.park_checked("n1", Some("not-the-token")),
            Err(TunnelAuthError::InvalidParkToken)
        ));
        assert!(matches!(
            reg.park_checked("n1", None),
            Err(TunnelAuthError::InvalidParkToken)
        ));
        assert_eq!(reg.len(), 1);
        assert!(!reg.is_live("n1"), "a rejected park must not go live");
        // The genuine credential still seats.
        assert!(reg.park_checked("n1", Some(&tok)).is_ok());
        assert!(reg.is_live("n1"));
    }

    /// A8: `forget` drops a node's tunnel state -- its token dies with the
    /// Node object, so a stale credential cannot re-park afterwards.
    #[test]
    fn forget_drops_the_node_tunnel_state() {
        let reg = TunnelRegistry::new_shared();
        let tok = reg.register("n1");
        assert!(reg.park_checked("n1", Some(&tok)).is_ok());
        assert!(reg.is_live("n1"));
        reg.forget("n1");
        assert_eq!(reg.len(), 0);
        assert!(!reg.is_live("n1"));
        // The pre-forget token is dead: the entry (and its parkToken) is gone.
        assert!(matches!(
            reg.park_checked("n1", Some(&tok)),
            Err(TunnelAuthError::InvalidParkToken)
        ));
        assert_eq!(
            reg.len(),
            0,
            "a rejected re-park must not resurrect the entry"
        );
        // Idempotent on an unknown node.
        reg.forget("n1");
        assert_eq!(reg.len(), 0);
        // Unrelated nodes are untouched.
        reg.register("n2");
        reg.forget("n1");
        assert_eq!(reg.len(), 1);
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
        let tok = reg.register("n1");
        let park = reg.park_checked("n1", Some(&tok)).unwrap();
        assert!(reg.is_live("n1"));

        // Fake agent: hold the long-poll, answer the handed-off request.
        let reg_agent = reg.clone();
        let tok_agent = tok.clone();
        let agent = tokio::spawn(async move {
            let request = park.rx.await.expect("parked poll receives request");
            assert_eq!(request.method, "POST");
            assert_eq!(request.path, "/pods/uid/containers/c/exec");
            assert_eq!(
                request.headers.get("content-type").map(String::as_str),
                Some("application/json")
            );
            assert!(reg_agent
                .complete_checked(
                    "n1",
                    &request.id,
                    Some(&tok_agent),
                    RelayResponse::from_reply(&reply(200, "ok"))
                )
                .unwrap());
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
        let tok = reg.register("n1");
        let park = reg.park_checked("n1", Some(&tok)).unwrap();
        // Agent parks but never answers.
        let err = reg
            .submit("n1", "GET", "/pods/u/containers/c/log", None, None)
            .await
            .unwrap_err();
        assert!(err.contains("timed out"), "{err}");
        // The stale pending entry must be gone; the parked poll still holds
        // the request, so completing late is a no-op (waiter left).
        drop(park);
        assert!(!reg
            .complete_checked(
                "n1",
                "bogus",
                Some(&tok),
                RelayResponse::from_reply(&reply(200, "late"))
            )
            .unwrap());
    }

    #[tokio::test]
    async fn cancelled_submit_cleans_pending() {
        // S53 / Q65 (review P1-②): a submit future CANCELLED before the
        // relay SLA (the HTTP caller disconnected and axum dropped the
        // handler) runs no outcome branch — the Drop guard must still
        // remove the pending responder instead of leaking it.
        let reg = TunnelRegistry::with_relay_timeout(Duration::from_secs(10));
        let tok = reg.register("n1");
        let park = reg.park_checked("n1", Some(&tok)).unwrap();
        // Outer window shorter than the relay SLA: when it elapses, the
        // timeout drops the inner submit future mid-flight.
        let cancelled = tokio::time::timeout(
            Duration::from_millis(50),
            reg.submit("n1", "GET", "/pods/u/containers/c/log", None, None),
        )
        .await;
        assert!(cancelled.is_err(), "outer timeout should win the race");
        let nodes = reg.nodes.lock().expect("tunnel registry poisoned");
        let entry = nodes.get("n1").expect("node entry remains");
        assert!(
            entry.pending.is_empty(),
            "cancelled submit leaked a pending responder"
        );
        drop(nodes);
        drop(park);
    }

    #[tokio::test]
    async fn unpark_generation_guard() {
        let reg = TunnelRegistry::new_shared();
        let tok = reg.register("n1");
        let old = reg.park_checked("n1", Some(&tok)).unwrap();
        let new = reg.park_checked("n1", Some(&tok)).unwrap(); // replaces the older poll
        reg.unpark("n1", old.generation); // stale end: must not clear `new`
        assert!(reg.is_live("n1"));
        reg.unpark("n1", new.generation);
        assert!(!reg.is_live("n1"));
    }

    #[tokio::test]
    async fn dead_park_self_heals() {
        let reg = TunnelRegistry::new_shared();
        let tok = reg.register("n1");
        let park = reg.park_checked("n1", Some(&tok)).unwrap();
        drop(park.rx); // client vanished mid-poll
        assert!(reg.is_live("n1"), "liveness is optimistic until send");
        let err = reg
            .submit("n1", "GET", "/pods/u/containers/c/log", None, None)
            .await
            .unwrap_err();
        assert!(err.contains("handoff failed"), "{err}");
        // The stale seat was consumed: not live, and a fresh park works.
        assert!(!reg.is_live("n1"));
        let fresh = reg.park_checked("n1", Some(&tok)).unwrap();
        assert!(reg.is_live("n1"));
        drop(fresh);
    }

    // ---------------------------------------- S53 / Q65 review P1-③ ---

    #[test]
    fn register_rotates_single_active_token() {
        let reg = TunnelRegistry::new_shared();
        let first = reg.register("n1");
        let second = reg.register("n1");
        for tok in [&first, &second] {
            assert_eq!(tok.len(), 32, "128-bit hex: {tok}");
            assert!(
                tok.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')),
                "lowercase hex: {tok}"
            );
        }
        assert_ne!(first, second, "every register rotates the token");
        // Single active value: the OLD token now fails park_checked.
        assert!(matches!(
            reg.park_checked("n1", Some(&first)),
            Err(TunnelAuthError::InvalidParkToken)
        ));
        assert!(!reg.is_live("n1"), "a rejected park must not seat");
        reg.park_checked("n1", Some(&second)).unwrap();
        assert!(reg.is_live("n1"));
    }

    #[test]
    fn park_rejects_missing_and_wrong_token() {
        let reg = TunnelRegistry::new_shared();
        let tok = reg.register("n1");
        // Missing (None) and mismatched are deliberately indistinguishable.
        assert!(matches!(
            reg.park_checked("n1", None),
            Err(TunnelAuthError::InvalidParkToken)
        ));
        assert!(matches!(
            reg.park_checked("n1", Some("wrong")),
            Err(TunnelAuthError::InvalidParkToken)
        ));
        // Never-registered node: the Default entry carries an empty token.
        assert!(matches!(
            reg.park_checked("n2", Some("anything")),
            Err(TunnelAuthError::InvalidParkToken)
        ));
        // Cross-node credential: n2's token does not open n1's seat.
        let tok2 = reg.register("n2");
        assert!(matches!(
            reg.park_checked("n1", Some(&tok2)),
            Err(TunnelAuthError::InvalidParkToken)
        ));
        assert!(!reg.is_live("n1"), "rejected parks must not seat");
        assert!(!reg.is_live("n2"));
        // The node's own token still seats after the rejections above.
        drop(reg.park_checked("n1", Some(&tok)).unwrap());
    }

    #[tokio::test]
    async fn complete_rejects_missing_and_wrong_token() {
        // Needs a live pending: park + submit (the `handoff_roundtrip`
        // flow), then exercise the agent-side completion checks.
        let reg = TunnelRegistry::new_shared();
        let tok = reg.register("n1");
        let park = reg.park_checked("n1", Some(&tok)).unwrap();

        let reg_agent = reg.clone();
        let tok_agent = tok.clone();
        let agent = tokio::spawn(async move {
            let request = park.rx.await.expect("parked poll receives request");
            // Missing and mismatched credentials are rejected and must NOT
            // resolve the waiter (the submit below would then observe the
            // "bad" body instead of "ok").
            assert_eq!(
                reg_agent.complete_checked(
                    "n1",
                    &request.id,
                    None,
                    RelayResponse::from_reply(&reply(200, "bad"))
                ),
                Err(TunnelAuthError::InvalidParkToken)
            );
            assert_eq!(
                reg_agent.complete_checked(
                    "n1",
                    &request.id,
                    Some("wrong"),
                    RelayResponse::from_reply(&reply(200, "bad"))
                ),
                Err(TunnelAuthError::InvalidParkToken)
            );
            // Cross-node credential: n2's token cannot resolve n1's waiter.
            let tok2 = reg_agent.register("n2");
            assert_eq!(
                reg_agent.complete_checked(
                    "n1",
                    &request.id,
                    Some(&tok2),
                    RelayResponse::from_reply(&reply(200, "bad"))
                ),
                Err(TunnelAuthError::InvalidParkToken)
            );
            // The valid token resolves it.
            assert!(reg_agent
                .complete_checked(
                    "n1",
                    &request.id,
                    Some(&tok_agent),
                    RelayResponse::from_reply(&reply(200, "ok"))
                )
                .unwrap());
        });

        let got = reg
            .submit("n1", "GET", "/pods/u/containers/c/log", None, None)
            .await
            .expect("relayed reply");
        // The waiter saw ONLY the valid token's response: the rejected
        // attempts never resolved it (otherwise the body would be "bad").
        assert_eq!(got.code, 200);
        assert_eq!(got.body, b"ok");
        agent.await.unwrap();
    }
}
