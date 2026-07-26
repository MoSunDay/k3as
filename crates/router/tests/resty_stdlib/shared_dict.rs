//! `ngx.shared.DICT` acceptance: 6 stateless sync tests + the **cross-request
//! persistence gate** (one logical in-process variant and one real-TCP
//! variant mirroring the T5.2 §6 gate).

use router::{worker_vm, Pipeline};
use tokio::task::LocalSet;

use super::helpers::{body_of, eval, get, http_get};

// ============================ ngx.shared.DICT ============================

#[test]
fn shared_dict_set_get_overwrite() {
    let (first, second): (String, String) = eval(
        "local d = ngx.shared.dogs d:set('k','one') local a=d:get('k') \
         d:set('k','two') return a, d:get('k')",
    );
    assert_eq!((first.as_str(), second.as_str()), ("one", "two"));
}

#[test]
fn shared_dict_set_returns_ok() {
    let (ok, err): (bool, Option<String>) =
        eval("local d = ngx.shared.s2 local ok,err = d:set('k',5) return ok,err");
    assert!(ok);
    assert!(err.is_none());
}

#[test]
fn shared_dict_inc_atomic_semantics() {
    // Single VM across chunks: the dict zone persists in app_data.
    let lua = worker_vm().expect("vm");
    // inc on a missing key with no init -> nil, "not found".
    let (none, err): (Option<f64>, Option<String>) =
        lua.load("return ngx.shared.s4:incr('ctr', 1)").eval().expect("e1");
    assert!(none.is_none());
    assert_eq!(err.as_deref(), Some("not found"));
    // inc with init seeds then adds.
    let (v1, _): (f64, Option<String>) =
        lua.load("return ngx.shared.s4:incr('ctr', 5, 100)").eval().expect("e2");
    assert_eq!(v1, 105.0);
    // subsequent inc accumulates on the same zone.
    let (v2, _): (f64, Option<String>) =
        lua.load("return ngx.shared.s4:incr('ctr', -2)").eval().expect("e3");
    assert_eq!(v2, 103.0);
}

#[test]
fn shared_dict_add_replace_semantics() {
    let (ok_add, err_add): (bool, Option<String>) = eval(
        "local d = ngx.shared.s5 d:set('k',1) local ok,err = d:add('k',2) return ok,err",
    );
    assert!(!ok_add);
    assert_eq!(err_add.as_deref(), Some("exists"));
    let (ok_rep_miss, err_rep): (bool, Option<String>) = eval(
        "local d = ngx.shared.s5 local ok,err = d:replace('missing',2) return ok,err",
    );
    assert!(!ok_rep_miss);
    assert_eq!(err_rep.as_deref(), Some("not found"));
    let (ok_rep_ok, val): (bool, Option<i64>) = eval(
        "local d = ngx.shared.s5 d:set('k',1) local ok=d:replace('k',42) return ok, d:get('k')",
    );
    assert!(ok_rep_ok);
    assert_eq!(val, Some(42));
}

#[test]
fn shared_dict_get_keys_and_get_all() {
    let (n_keys, all_a): (usize, Option<i64>) = eval(
        "local d = ngx.shared.s6 d:set('a',1) d:set('b',2) \
         return #d:get_keys(), d:get_all().a",
    );
    assert_eq!(n_keys, 2);
    assert_eq!(all_a, Some(1));
}

#[test]
fn shared_dict_flush_all_clears() {
    let n: i64 = eval(
        "local d = ngx.shared.s7 d:set('a',1) d:flush_all() return #d:get_keys(0)",
    );
    assert_eq!(n, 0);
}

// ============ THE T5.3 GATE: shared dict persists across requests ============

/// Content that branches on URI: `/write` stores into `ngx.shared.gate`, else
/// reads + says it.
const GATE_CONTENT: &str = r#"return function()
  if ngx.var.uri == "/write" then
    ngx.shared.gate:set("key", "persisted-across-requests")
    ngx.say("wrote")
  else
    ngx.say(ngx.shared.gate:get("key") or "MISSING")
  end
end"#;

/// LOGICAL gate: two sequential in-process requests on ONE VM observe the same
/// `ngx.shared.DICT` entry (A writes, B reads).
#[tokio::test]
async fn shared_dict_persists_across_requests_logical() {
    LocalSet::new()
        .run_until(async {
            let p = Pipeline::build().content(GATE_CONTENT).try_build().expect("pipeline");
            assert_eq!(body_of(&p, get("/write")).await, "wrote\n");
            assert_eq!(
                body_of(&p, get("/read")).await,
                "persisted-across-requests\n",
                "request B must observe request A's shared-dict write"
            );
        })
        .await;
}

/// REAL-TCP gate: two real HTTP requests over a live socket share the dict.
#[tokio::test]
async fn shared_dict_persists_across_requests_real_tcp() {
    LocalSet::new()
        .run_until(async {
            let p = Pipeline::build().content(GATE_CONTENT).try_build().expect("pipeline");
            let (addr, listener) = router::ephemeral_listener().expect("listener");
            let (tx, rx) = tokio::sync::oneshot::channel::<()>();
            let server = tokio::task::spawn_local(router::serve(p, listener, async {
                let _ = rx.await;
            }));
            let body_a = http_get(addr, "/write").await.2;
            let body_b = http_get(addr, "/read").await.2;
            let _ = tx.send(());
            let _ = server.await;
            assert_eq!(String::from_utf8_lossy(&body_a), "wrote\n");
            assert_eq!(
                String::from_utf8_lossy(&body_b),
                "persisted-across-requests\n",
                "real client request B must observe request A's shared-dict write"
            );
        })
        .await;
}
