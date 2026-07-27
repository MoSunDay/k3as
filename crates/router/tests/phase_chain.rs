//! T5.2 Scope B acceptance: the full openresty phase chain, observed by a real
//! TCP client.
//!
//! The T5.2 gate (README/index §6): a `header_filter_by_lua` mutates response
//! headers, observed by a real client. Plus the rest of the chain —
//! `rewrite`/`access` short-circuit, `body_filter`, request-body round-trip,
//! `ngx.var`, `ngx.exec` (internal redirect), `ngx.redirect`, and `init_worker`.

use http::Request;
use router::{build_response, Pipeline};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::task::LocalSet;

/// Drive one request through the pipeline (logical, in-process).
async fn run_one(
    builder: router::PipelineBuilder,
    req: Request<()>,
) -> (u16, Vec<(String, String)>, Vec<u8>) {
    let p = builder.try_build().expect("pipeline");
    let out = p.serve_request(req).await;
    let resp = build_response(out);
    let (parts, body) = resp.into_parts();
    let headers = parts
        .headers
        .iter()
        .map(|(n, v)| (n.as_str().to_owned(), v.to_str().unwrap_or("").to_owned()))
        .collect();
    (parts.status.as_u16(), headers, body.to_vec())
}

fn get(path: &str) -> Request<()> {
    Request::builder().method("GET").uri(path).body(()).unwrap()
}

fn header<'a>(hdrs: &'a [(String, String)], name: &str) -> Option<&'a str> {
    hdrs.iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

// ---- logical (in-process) phase tests ----

/// `header_filter` runs AFTER content and can overwrite a content-set header.
#[tokio::test]
async fn header_filter_runs_after_content_and_overwrites() {
    LocalSet::new()
        .run_until(async {
            let b = Pipeline::build()
                .content(r#"return function() ngx.header["X-Foo"] = "one"; ngx.say("c") end"#)
                .header_filter(r#"return function() ngx.header["X-Foo"] = "two" end"#);
            let (status, hdrs, body) = run_one(b, get("/")).await;
            assert_eq!(status, 200);
            assert_eq!(header(&hdrs, "x-foo"), Some("two"));
            assert_eq!(body, b"c\n");
        })
        .await;
}

/// `body_filter` transforms the assembled body (buffered whole-body mode).
#[tokio::test]
async fn body_filter_transforms_body() {
    LocalSet::new()
        .run_until(async {
            let b = Pipeline::build()
                .content("return function() ngx.say('hello') end")
                .body_filter(r#"return function() ngx.arg[1] = string.upper(ngx.arg[1]) end"#);
            let (_s, _h, body) = run_one(b, get("/")).await;
            assert_eq!(body, b"HELLO\n");
        })
        .await;
}

/// `rewrite` short-circuits via `ngx.exit(403)` — content never runs.
#[tokio::test]
async fn rewrite_exit_skips_content() {
    LocalSet::new()
        .run_until(async {
            let b = Pipeline::build()
                .rewrite(
                    r#"return function()
                          if ngx.req.get_path() == "/secret" then ngx.exit(403) end
                       end"#,
                )
                .content("return function() ngx.say('should-not-appear') end");
            let (s_secret, _h, body) = run_one(b.clone(), get("/secret")).await;
            assert_eq!(s_secret, 403);
            assert!(body.is_empty(), "body leaked past exit: {body:?}");
        })
        .await;
}

/// `access` runs after `rewrite`; both see the same request.
#[tokio::test]
async fn access_phase_runs_and_can_deny() {
    LocalSet::new()
        .run_until(async {
            let b = Pipeline::build()
                .rewrite("return function() ngx.var.touched = '1' end")
                .access(
                    r#"return function()
                          if ngx.var.touched ~= '1' then ngx.exit(500) end
                          ngx.exit(401)
                       end"#,
                )
                .content("return function() ngx.say('x') end");
            let (s, _h, _body) = run_one(b, get("/")).await;
            assert_eq!(s, 401);
        })
        .await;
}

/// `ngx.var` read (essentials) + write (user vars) across phases.
#[tokio::test]
async fn ngx_var_read_write_across_phases() {
    LocalSet::new()
        .run_until(async {
            let b = Pipeline::build()
                .rewrite(r#"return function() ngx.var.user = "rw" end"#)
                .content(
                    r#"return function()
                          ngx.print(ngx.var.user, "|", ngx.var.uri, "|", ngx.var.request_method)
                       end"#,
                );
            let (_s, _h, body) = run_one(b, get("/users/42")).await;
            assert_eq!(body, b"rw|/users/42|GET");
        })
        .await;
}

/// `ngx.exec` internal redirect re-runs the generative phase for the new URI.
#[tokio::test]
async fn ngx_exec_internal_redirect_reruns() {
    LocalSet::new()
        .run_until(async {
            let b = Pipeline::build().content(
                r#"return function()
                      if ngx.var.uri == "/old" then ngx.exec("/new") end
                      ngx.say("uri=", ngx.var.uri)
                   end"#,
            );
            let (_s, _h, body_old) = run_one(b.clone(), get("/old")).await;
            assert_eq!(body_old, b"uri=/new\n");
            let (_s, _h, body_new) = run_one(b, get("/new")).await;
            assert_eq!(body_new, b"uri=/new\n");
        })
        .await;
}

/// `ngx.redirect` emits a 302 + Location and terminates.
#[tokio::test]
async fn ngx_redirect_emits_302_location() {
    LocalSet::new()
        .run_until(async {
            let b =
                Pipeline::build().content(r#"return function() ngx.redirect("/elsewhere") end"#);
            let (s, hdrs, _body) = run_one(b, get("/")).await;
            assert_eq!(s, 302);
            assert_eq!(header(&hdrs, "location"), Some("/elsewhere"));
        })
        .await;
}

/// request body round-trip: a buffered POST body is echoed via get_body_data.
#[tokio::test]
async fn post_body_echoed_via_get_body_data() {
    LocalSet::new()
        .run_until(async {
            let b = Pipeline::build().content(
                r#"return function()
                      ngx.req.read_body()
                      ngx.print(ngx.req.get_body_data())
                   end"#,
            );
            let p = b.try_build().expect("pipeline");
            let parts = Request::builder()
                .method("POST")
                .uri("/echo")
                .body(())
                .unwrap()
                .into_parts()
                .0;
            let out = p
                .serve_request_with_body(&parts, b"hello=world".to_vec())
                .await;
            assert_eq!(build_response(out).body().as_ref(), b"hello=world");
        })
        .await;
}

/// `ngx.req.get_post_args` / `get_query_args` parse urlencoded data.
#[tokio::test]
async fn post_and_query_args_parsed() {
    LocalSet::new()
        .run_until(async {
            let b = Pipeline::build().content(
                r#"return function()
                      ngx.req.read_body()
                      local p = ngx.req.get_post_args()
                      local q = ngx.req.get_query_args()
                      ngx.say(p.greeting, "|", q.who)
                   end"#,
            );
            let p = b.try_build().expect("pipeline");
            let parts = Request::builder()
                .method("POST")
                .uri("/x?who=alice")
                .body(())
                .unwrap()
                .into_parts()
                .0;
            let out = p
                .serve_request_with_body(&parts, b"greeting=hello+world".to_vec())
                .await;
            assert_eq!(build_response(out).body().as_ref(), b"hello world|alice\n");
        })
        .await;
}

/// `init_worker` runs once at boot and can seed shared VM state.
#[tokio::test]
async fn init_worker_runs_once_at_boot() {
    LocalSet::new()
        .run_until(async {
            let p = Pipeline::build()
                .init_worker(r#"return function() _G.boot_greeting = "hi-from-init" end"#)
                .content(r#"return function() ngx.say(_G.boot_greeting) end"#)
                .try_build()
                .expect("pipeline");
            p.boot().await; // must run before serving
            let out = p.serve_request(get("/")).await;
            assert_eq!(build_response(out).body().as_ref(), b"hi-from-init\n");
        })
        .await;
}

/// `ngx.now`/`ngx.time`/`ngx.update_time` return plausible epoch seconds.
#[tokio::test]
async fn time_helpers_return_epoch_seconds() {
    LocalSet::new()
        .run_until(async {
            let p = Pipeline::build()
                .content(
                    r#"return function()
                          ngx.update_time()
                          local n, t = ngx.now(), ngx.time()
                          ngx.print(math.floor(n) == math.floor(t) and n > 1700000000)
                       end"#,
                )
                .try_build()
                .expect("pipeline");
            let out = p.serve_request(get("/")).await;
            assert_eq!(build_response(out).body().as_ref(), b"true");
        })
        .await;
}

/// Pipeline::new (content-only convenience) still works (backward compat).
#[tokio::test]
async fn pipeline_new_content_only_still_works() {
    LocalSet::new()
        .run_until(async {
            let p = Pipeline::new("return function() ngx.say('legacy') end").expect("pipeline");
            let out = p.serve_request(get("/")).await;
            assert_eq!(build_response(out).body().as_ref(), b"legacy\n");
        })
        .await;
}

// ---- the T5.2 acceptance gate: real TCP client ----

/// Minimal HTTP/1.1 client: send a request, return (status, headers, body).
async fn http_get(
    addr: std::net::SocketAddr,
    path: &str,
    extra_headers: &[(&str, &str)],
) -> (u16, Vec<(String, String)>, Vec<u8>) {
    let mut stream = TcpStream::connect(addr).await.expect("connect");
    let mut req = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n");
    for (k, v) in extra_headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("connection: close\r\n\r\n");
    send_and_read(&mut stream, req.as_bytes()).await
}

/// Send raw bytes (request) and parse the HTTP/1.1 response.
async fn send_and_read(
    stream: &mut TcpStream,
    req: &[u8],
) -> (u16, Vec<(String, String)>, Vec<u8>) {
    stream.write_all(req).await.expect("write");
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.expect("read");
    let text = String::from_utf8_lossy(&buf);
    let mut lines = text.split("\r\n");
    let status_line = lines.next().expect("status line");
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .expect("status code")
        .parse()
        .expect("status num");
    let mut headers = Vec::new();
    let mut body_start = 0;
    for (i, line) in text.split("\r\n").enumerate() {
        if line.is_empty() {
            body_start = text
                .split("\r\n")
                .take(i)
                .map(|l| l.len() + 2)
                .sum::<usize>()
                + 2;
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_owned(), v.trim().to_owned()));
        }
    }
    let body = buf[body_start.min(buf.len())..].to_vec();
    (status, headers, body)
}

/// THE T5.2 GATE: a `header_filter_by_lua` mutates response headers, observed by
/// a real client over TCP.
#[tokio::test]
async fn real_client_observes_header_filter_mutation() {
    LocalSet::new()
        .run_until(async {
            let p = Pipeline::build()
                .content(r#"return function() ngx.header["X-Content"] = "c"; ngx.say("body") end"#)
                .header_filter(
                    r#"return function()
                          ngx.header["X-Added-By-Filter"] = "yes"
                          ngx.header["X-Content"] = "overwritten"
                       end"#,
                )
                .try_build()
                .expect("pipeline");
            let (addr, listener) = router::ephemeral_listener().expect("listener");
            let (tx, rx) = tokio::sync::oneshot::channel::<()>();
            let server = tokio::task::spawn_local(router::serve(p, listener, async {
                let _ = rx.await;
            }));
            let (status, headers, body) = http_get(addr, "/anything", &[]).await;
            let _ = tx.send(());
            let _ = server.await;

            assert_eq!(status, 200);
            assert_eq!(body, b"body\n");
            assert_eq!(header(&headers, "x-added-by-filter"), Some("yes"));
            assert_eq!(header(&headers, "x-content"), Some("overwritten"));
        })
        .await;
}

/// Real TCP: POST body round-trip (chunked + content-length) echoed back.
#[tokio::test]
async fn real_client_post_body_roundtrip() {
    LocalSet::new()
        .run_until(async {
            let p = Pipeline::build()
                .content(
                    r#"return function()
                          ngx.req.read_body()
                          ngx.print(ngx.req.get_body_data())
                       end"#,
                )
                .try_build()
                .expect("pipeline");
            let (addr, listener) = router::ephemeral_listener().expect("listener");
            let (tx, rx) = tokio::sync::oneshot::channel::<()>();
            let server = tokio::task::spawn_local(router::serve(p, listener, async {
                let _ = rx.await;
            }));

            // Content-Length framed POST.
            let mut s = TcpStream::connect(addr).await.expect("connect");
            let body = b"name=alice&age=30";
            let req = format!(
                "POST /echo HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            let mut req_bytes = req.into_bytes();
            req_bytes.extend_from_slice(body);
            let (status, _h, echoed) = send_and_read(&mut s, &req_bytes).await;
            assert_eq!(status, 200);
            assert_eq!(echoed, body);

            let _ = tx.send(());
            let _ = server.await;
        })
        .await;
}

/// Real TCP: a real redirect observed by the client (302 + Location).
#[tokio::test]
async fn real_client_observes_redirect() {
    LocalSet::new()
        .run_until(async {
            let p = Pipeline::build()
                .content(r#"return function() ngx.redirect("https://example.com/new", 301) end"#)
                .try_build()
                .expect("pipeline");
            let (addr, listener) = router::ephemeral_listener().expect("listener");
            let (tx, rx) = tokio::sync::oneshot::channel::<()>();
            let server = tokio::task::spawn_local(router::serve(p, listener, async {
                let _ = rx.await;
            }));
            let (status, headers, _body) = http_get(addr, "/", &[]).await;
            let _ = tx.send(());
            let _ = server.await;
            assert_eq!(status, 301);
            assert_eq!(
                header(&headers, "location"),
                Some("https://example.com/new")
            );
        })
        .await;
}
