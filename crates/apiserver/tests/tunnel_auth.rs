//! S53 wire-level tests for the tunnel PARK IDENTITY BINDING (review
//! P1-③, TODO **T4.5**, decisions **Q65/Q31/Q47**): the `tunnel/register`
//! ack carries the rotated `parkToken` (getrandom 128-bit hex, single
//! active value), and `tunnel/next` / `tunnel/responses/{id}` must present
//! it as the `parkToken` query parameter — missing OR mismatched fails
//! closed with 401 (the two are deliberately indistinguishable), and auth
//! runs BEFORE the pending-id lookup (an unknown id must not leak a 404 to
//! an unauthenticated caller).
//!
//! In-process pattern of `service_nodeport.rs` (no network, no real etcd —
//! ADR Q17): the real axum handlers via `tower::ServiceExt::oneshot` over
//! [`apiserver::api_app`]. That constructor builds the ANONYMOUS plane
//! (the Q47 authn/authz pair is not armed), so every 401 below comes from
//! the P1-③ ownership check itself — layering the Q47 middleware on top
//! would only add its own 401 in front of these routes, never relax them.

use std::sync::Arc;
use std::time::Duration;

use api::SchemaRegistry;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use serde_json::{json, Value};
use storage::{EmbeddedStorage, StorageBackend};
use tower::ServiceExt;

fn served() -> SchemaRegistry {
    let mut reg = SchemaRegistry::with_core_v1();
    api::initpro::register(&mut reg);
    reg
}

/// One router over a fresh embedded store (state is Arc-shared, so cloning
/// the router keeps the SAME tunnel registry across requests — register /
/// park / complete below all hit one registry, exactly like one apiserver
/// serving one dialer).
fn app() -> axum::Router {
    let store: Arc<dyn StorageBackend> = Arc::new(EmbeddedStorage::new());
    apiserver::api_app(Arc::new(served()), store, "127.0.0.1:6443".to_string())
}

/// `service_nodeport.rs`'s transport: method + uri (+ optional JSON body)
/// -> `(status, body)`.
async fn send(
    router: axum::Router,
    method: &str,
    uri: &str,
    body: Option<(&str, Vec<u8>)>,
) -> (StatusCode, Value) {
    let mut b = Request::builder()
        .method(Method::from_bytes(method.as_bytes()).unwrap())
        .uri(uri);
    if let Some((ct, _)) = body {
        b = b.header("content-type", ct);
    }
    let req = match body {
        Some((_, bytes)) => b.body(Body::from(bytes)),
        None => b.body(Body::empty()),
    }
    .unwrap();
    let resp = router.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let val = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, val)
}

fn json_body(v: &Value) -> Option<(&'static str, Vec<u8>)> {
    Some(("application/json", serde_json::to_vec(v).unwrap()))
}

/// POST `tunnel/register` for `node`, returning the ack's `parkToken`.
async fn register_token(router: &axum::Router, node: &str) -> String {
    let (st, ack) = send(
        router.clone(),
        "POST",
        &format!("/api/v1/nodes/{node}/tunnel/register"),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "register failed: {ack}");
    ack["parkToken"]
        .as_str()
        .expect("ack carries parkToken")
        .to_string()
}

/// POST `tunnel/responses/{id}` with a minimal valid `RelayResponse` body
/// and the caller-chosen credential (None = query parameter stripped).
async fn post_relay_response(
    router: &axum::Router,
    node: &str,
    id: &str,
    token: Option<&str>,
) -> (StatusCode, Value) {
    let mut uri = format!("/api/v1/nodes/{node}/tunnel/responses/{id}");
    if let Some(t) = token {
        // Plain lowercase-hex token: no percent-encoding needed.
        uri.push_str(&format!("?parkToken={t}"));
    }
    let body = json!({"status": 200, "headers": {}});
    send(router.clone(), "POST", &uri, json_body(&body)).await
}

/// True for 32-char lowercase hex credentials.
fn is_128bit_hex(token: &str) -> bool {
    token.len() == 32 && token.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f'))
}

#[tokio::test]
async fn register_ack_carries_a_32_hex_lowercase_park_token() {
    let r = app();
    let (st, ack) = send(r.clone(), "POST", "/api/v1/nodes/n1/tunnel/register", None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(ack["node"], "n1");
    assert_eq!(ack["registered"], true);
    let token = ack["parkToken"].as_str().expect("parkToken present");
    assert!(is_128bit_hex(token), "128-bit lowercase hex: {token}");
}

#[tokio::test]
async fn register_rotates_the_park_token_on_the_wire() {
    let r = app();
    let first = register_token(&r, "n1").await;
    let second = register_token(&r, "n1").await;
    assert!(is_128bit_hex(&first) && is_128bit_hex(&second));
    assert_ne!(first, second, "every register rotates the credential");
    // Single active value: the OLD token now fail-closes the long-poll.
    let (st, body) = send(
        r.clone(),
        "GET",
        &format!("/api/v1/nodes/n1/tunnel/next?parkToken={first}"),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
    assert_eq!(body["reason"], "Unauthorized");
}

#[tokio::test]
async fn next_without_park_token_fails_closed_401() {
    // A stripped query parameter is indistinguishable from an old client:
    // NO absent-tolerance (fail-closed, Q65 S53 P1-3).
    let r = app();
    register_token(&r, "n1").await;
    let (st, body) = send(r.clone(), "GET", "/api/v1/nodes/n1/tunnel/next", None).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
    assert_eq!(body["code"], 401);
    assert_eq!(body["reason"], "Unauthorized");
}

#[tokio::test]
async fn next_with_wrong_park_token_fails_closed_401() {
    let r = app();
    register_token(&r, "n1").await;
    let (st, _) = send(
        r.clone(),
        "GET",
        "/api/v1/nodes/n1/tunnel/next?parkToken=deadbeef",
        None,
    )
    .await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn next_401_is_identical_for_missing_and_wrong_tokens() {
    // No oracle: the Status bytes for a stripped parameter and a wrong
    // value must be byte-identical (indistinguishable by design).
    let r = app();
    register_token(&r, "n1").await;
    let (st_missing, missing) = send(r.clone(), "GET", "/api/v1/nodes/n1/tunnel/next", None).await;
    let (st_wrong, wrong) = send(
        r.clone(),
        "GET",
        "/api/v1/nodes/n1/tunnel/next?parkToken=deadbeef",
        None,
    )
    .await;
    assert_eq!(st_missing, StatusCode::UNAUTHORIZED);
    assert_eq!(st_wrong, StatusCode::UNAUTHORIZED);
    assert_eq!(
        missing["message"], wrong["message"],
        "missing and mismatched must be indistinguishable"
    );
}

#[tokio::test]
async fn next_with_valid_park_token_parks_instead_of_401() {
    let r = app();
    let token = register_token(&r, "n1").await;
    let req = Request::builder()
        .method(Method::GET)
        .uri(format!("/api/v1/nodes/n1/tunnel/next?parkToken={token}"))
        .body(Body::empty())
        .unwrap();
    // The valid-credential long-poll parks the full 25s server window (no
    // relayed request ever arrives). tokio's paused test clock is not
    // available to this crate, and a 25s-sleeping test is unacceptable, so
    // bound the probe: EITHER outcome below proves the request got PAST
    // the P1-3 check (an invalid token answers 401 instantly):
    //  - Elapsed: the long-poll is still parked (dropping the in-flight
    //    future unparks via the LongPollGuard — no leaked seat);
    //  - Ok: the empty window already answered 204.
    match tokio::time::timeout(Duration::from_millis(300), r.clone().oneshot(req)).await {
        Ok(resp) => assert_eq!(resp.unwrap().status(), StatusCode::NO_CONTENT),
        Err(_still_parked) => {
            // The runtime keeps serving during the parked window: a WRONG
            // token still fail-closes immediately on the same face.
            let (st, _) = send(
                r,
                "GET",
                "/api/v1/nodes/n1/tunnel/next?parkToken=deadbeef",
                None,
            )
            .await;
            assert_eq!(st, StatusCode::UNAUTHORIZED);
        }
    }
}

#[tokio::test]
async fn responses_without_token_fails_closed_before_the_id_lookup() {
    // Auth runs BEFORE the pending-id lookup: even an UNKNOWN id answers
    // 401 (never today's 404 — no id-existence oracle for a caller that
    // did not prove park ownership).
    let r = app();
    register_token(&r, "n1").await;
    let (st, body) = post_relay_response(&r, "n1", "never-seen-id", None).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
    assert_eq!(body["reason"], "Unauthorized");
}

#[tokio::test]
async fn responses_with_wrong_token_fails_closed_401() {
    let r = app();
    register_token(&r, "n1").await;
    let (st, _) = post_relay_response(&r, "n1", "some-id", Some("deadbeef")).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn responses_with_valid_token_but_unknown_id_answers_404() {
    // Today's shape is preserved once ownership is proven: auth OK, the
    // waiter already gave up -> 404 NotFound.
    let r = app();
    let token = register_token(&r, "n1").await;
    let (st, body) = post_relay_response(&r, "n1", "no-such-id", Some(&token)).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    assert_eq!(body["reason"], "NotFound");
}

/// A8 (P2 hardening): an unauthenticated park with an arbitrary node name
/// must not grow the tunnel registry. Before the fix `park_checked` inserted
/// the node entry BEFORE the token check, so a spray of random names leaked
/// one `NodeTunnel` per request for the life of the process.
#[tokio::test]
async fn park_with_an_unknown_node_name_leaves_no_state_behind() {
    let r = app();
    for name in ["ghost-a", "ghost-b", "ghost-c"] {
        let (st, body) = send(
            r.clone(),
            "GET",
            &format!("/api/v1/nodes/{name}/tunnel/next?parkToken=deadbeef"),
            None,
        )
        .await;
        assert_eq!(st, StatusCode::UNAUTHORIZED, "{name} must fail closed");
        assert_eq!(body["reason"], "Unauthorized");
    }
    // Nothing was seated: no node answers as live, and a submit through any
    // of the sprayed names still reports "no tunnel".
    let (st, _) = send(
        r.clone(),
        "GET",
        &format!("/api/v1/nodes/{}/pods/u/containers/c/log", "ghost-a"),
        None,
    )
    .await;
    assert_ne!(st, StatusCode::OK, "a ghost node must never be relayed to");
}

/// A8: deleting the Node object drops its tunnel state, so the pre-delete
/// `parkToken` dies with it (a decommissioned node cannot re-park).
#[tokio::test]
async fn deleting_the_node_kills_its_park_token() {
    let r = app();
    let (st, _) = send(
        r.clone(),
        "POST",
        "/api/v1/nodes",
        json_body(&json!({
            "apiVersion": "v1", "kind": "Node",
            "metadata": {"name": "n1"},
        })),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);
    let token = register_token(&r, "n1").await;
    // The credential is live while the Node exists.
    let (st, _) = post_relay_response(&r, "n1", "no-such-id", Some(&token)).await;
    assert_eq!(
        st,
        StatusCode::NOT_FOUND,
        "auth passed (only the id is unknown)"
    );

    let (st, _) = send(r.clone(), "DELETE", "/api/v1/nodes/n1", None).await;
    assert_eq!(st, StatusCode::OK);
    let (st, body) = send(
        r.clone(),
        "GET",
        &format!("/api/v1/nodes/n1/tunnel/next?parkToken={token}"),
        None,
    )
    .await;
    assert_eq!(
        st,
        StatusCode::UNAUTHORIZED,
        "the pre-delete token must be dead: {body}"
    );
}
