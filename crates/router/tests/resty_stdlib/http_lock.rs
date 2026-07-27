//! `resty.http` (T5.3 Scope B) and `resty.lock` acceptance: 3 async HTTP
//! socket tests + 2 async mutual-exclusion tests.

use router::{build_server_config, worker_vm, CertKey};
use tokio::task::LocalSet;

use super::helpers::{gen_test_cert, spawn_responder};

// ============================ resty.http (T5.3 Scope B) ============================

#[tokio::test]
async fn http_request_uri_plaintext_get() {
    LocalSet::new()
        .run_until(async {
            let (addr, tx) = spawn_responder(None, "hello-world").await;
            let lua = worker_vm().expect("vm");
            let f = lua
                .load(
                    r#"
                    return function(host, port)
                        local r, err = resty.http.request_uri("http://"..host..":"..port.."/items")
                        assert(err == nil, tostring(err))
                        return r.status, r.body, r.headers["content-type"], r.headers["x-greet"]
                    end
                    "#,
                )
                .eval::<mlua::Function>()
                .expect("fn");
            let (status, body, ct, greet): (u16, String, String, String) = f
                .call_async(("127.0.0.1", addr.port()))
                .await
                .expect("call");
            let _ = tx.send(());
            assert_eq!(status, 200);
            assert_eq!(body, "hello-world");
            assert_eq!(ct, "text/plain");
            assert_eq!(greet, "hi");
        })
        .await;
}

#[tokio::test]
async fn http_post_request_sends_body() {
    LocalSet::new()
        .run_until(async {
            let (addr, tx) = spawn_responder(None, "posted-ok").await;
            let lua = worker_vm().expect("vm");
            let f = lua
                .load(
                    r#"
                    return function(host, port)
                        local r = resty.http.request_uri(
                            "http://"..host..":"..port.."/submit",
                            { method = "POST", body = "ping=42" })
                        return r.status, r.body
                    end
                    "#,
                )
                .eval::<mlua::Function>()
                .expect("fn");
            let (status, body): (u16, String) = f
                .call_async(("127.0.0.1", addr.port()))
                .await
                .expect("call");
            let _ = tx.send(());
            assert_eq!(status, 200);
            assert_eq!(body, "posted-ok");
        })
        .await;
}

#[tokio::test]
async fn http_request_uri_https_tls() {
    LocalSet::new()
        .run_until(async {
            // Register the cert as the DEFAULT (empty SNI host): the client
            // connects to the IP literal, which rustls sends WITHOUT an SNI
            // extension (RFC 6066 forbids IPs in SNI), so the resolver must
            // fall back to its default cert. verify=false skips SAN checks.
            let cert = gen_test_cert("127.0.0.1");
            let cfg = build_server_config(&[(
                String::new(),
                CertKey::pem(cert.cert_pem.clone(), cert.key_pem.clone()),
            )])
            .expect("server cfg");
            let (addr, tx) = spawn_responder(Some(cfg), "secure-body").await;
            let lua = worker_vm().expect("vm");
            let f = lua
                .load(
                    r#"
                    return function(host, port)
                        local r, err = resty.http.request_uri(
                            "https://"..host..":"..port.."/x", { verify = false })
                        assert(err == nil, tostring(err))
                        return r.status, r.body
                    end
                    "#,
                )
                .eval::<mlua::Function>()
                .expect("fn");
            let (status, body): (u16, String) = f
                .call_async(("127.0.0.1", addr.port()))
                .await
                .expect("call");
            let _ = tx.send(());
            assert_eq!(status, 200);
            assert_eq!(body, "secure-body");
        })
        .await;
}

// ============================ resty.lock (T5.3 Scope B) ============================

#[tokio::test]
async fn lock_acquire_release_and_reacquire() {
    LocalSet::new()
        .run_until(async {
            let lua = worker_vm().expect("vm");
            let f = lua
                .load(
                    r#"
                    return function()
                        local l = resty.lock.new("z1", { timeout = 500, exptime = 5 })
                        local elapsed, err = l:lock("alpha")
                        assert(elapsed ~= nil, err or "no elapsed")
                        assert(l:unlock())
                        -- re-acquire after release must succeed immediately
                        local e2, err2 = l:lock("alpha")
                        assert(e2 ~= nil, err2 or "reacquire failed")
                        l:unlock()
                        return "ok"
                    end
                    "#,
                )
                .eval::<mlua::Function>()
                .expect("fn");
            let out: String = f.call_async(()).await.expect("call");
            assert_eq!(out, "ok");
        })
        .await;
}

#[tokio::test]
async fn lock_mutual_exclusion_across_coroutines() {
    LocalSet::new()
        .run_until(async {
            let lua = worker_vm().expect("vm");
            // Holder A: grab "shared", mark the critical section, hold briefly,
            // then clear the marker and release.
            let fa = lua
                .load(
                    r#"
                    return function()
                        local l = resty.lock.new("mx", { timeout = 2000, exptime = 5 })
                        l:lock("shared")
                        ngx.shared.dogs:set("inside", "yes")
                        ngx.sleep(0.15)
                        ngx.shared.dogs:set("inside", "no")
                        l:unlock()
                        return "A"
                    end
                    "#,
                )
                .eval::<mlua::Function>()
                .expect("fa");
            // Waiter B: start slightly after A, block on "shared" until A
            // releases, then report the marker it observed. If the lock failed,
            // B would run while A still holds -> "yes".
            let fb = lua
                .load(
                    r#"
                    return function()
                        local l = resty.lock.new("mx", { timeout = 2000, exptime = 5 })
                        ngx.sleep(0.03)
                        l:lock("shared")
                        local v = ngx.shared.dogs:get("inside")
                        l:unlock()
                        return v
                    end
                    "#,
                )
                .eval::<mlua::Function>()
                .expect("fb");
            let (ra, rb) = tokio::join!(fa.call_async::<String>(()), fb.call_async::<String>(()));
            assert_eq!(ra.expect("A"), "A");
            // B only acquired after A released -> marker already cleared to "no".
            assert_eq!(
                rb.expect("B"),
                "no",
                "waiter must not enter the critical section while held"
            );
        })
        .await;
}
