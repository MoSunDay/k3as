//! T5.2a acceptance: a `content_by_lua` writes status + body, and headers,
//! observed by a **real HTTP client** over TCP. Also proves the Q13
//! coroutine-local binding holds under concurrent interleaving (the hardest
//! case: two requests' coroutines share the VM and interleave at awaits, yet
//! each resolves to its own request).

use bytes::Bytes;
use http::{Request, StatusCode};
use router::{build_response, Pipeline};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::task::LocalSet;

/// Concurrently drive two borrowed futures to completion (no `'static` need):
/// a dependency-free analogue of `futures::future::join` for the shared-VM case.
async fn join2<A, B>(a: A, b: B) -> (A::Output, B::Output)
where
    A: std::future::Future,
    B: std::future::Future,
{
    use std::future::Future;
    use std::pin::Pin;
    use std::task::{Context, Poll};
    let mut a: Pin<Box<dyn Future<Output = A::Output>>> = Box::pin(a);
    let mut b: Pin<Box<dyn Future<Output = B::Output>>> = Box::pin(b);
    std::future::poll_fn(move |cx: &mut Context<'_>| {
        let ra = a.as_mut().poll(cx);
        let rb = b.as_mut().poll(cx);
        match (ra, rb) {
            (Poll::Ready(x), Poll::Ready(y)) => Poll::Ready((x, y)),
            _ => Poll::Pending,
        }
    })
    .await
}

/// Drive one request through the pipeline and build its HTTP response.
async fn run_one(src: &str, req: Request<()>) -> (StatusCode, Vec<(String, String)>, Bytes) {
    let p = Pipeline::new(src).expect("pipeline");
    let out = p.serve_request(req).await;
    let resp = build_response(out);
    let (parts, body) = resp.into_parts();
    let headers = parts
        .headers
        .iter()
        .map(|(n, v)| (n.as_str().to_owned(), v.to_str().unwrap_or("").to_owned()))
        .collect();
    (parts.status, headers, body)
}

fn get(path: &str) -> Request<()> {
    Request::builder().method("GET").uri(path).body(()).unwrap()
}

// ---- logical (in-process) tests ----

#[tokio::test]
async fn content_writes_status_and_body() {
    LocalSet::new()
        .run_until(async {
            let src = r#"
                return function()
                    ngx.status = 201
                    ngx.say("hello from lua")
                end
            "#;
            let (status, _h, body) = run_one(src, get("/x")).await;
            assert_eq!(status, StatusCode::CREATED);
            assert_eq!(body.as_ref(), b"hello from lua\n");
        })
        .await;
}

#[tokio::test]
async fn ngx_header_is_settable() {
    LocalSet::new()
        .run_until(async {
            let src = r#"
                return function()
                    ngx.header["Content-Type"] = "application/json"
                    ngx.print('{"ok":true}')
                end
            "#;
            let (status, headers, body) = run_one(src, get("/")).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(
                headers
                    .iter()
                    .find(|(n, _)| n == "content-type")
                    .map(|(_, v)| v.clone()),
                Some("application/json".to_owned())
            );
            assert_eq!(body.as_ref(), br#"{"ok":true}"#);
        })
        .await;
}

#[tokio::test]
async fn ngx_exit_terminates_and_sets_status() {
    LocalSet::new()
        .run_until(async {
            // code after ngx.exit must NOT run.
            let src = r#"
                return function()
                    ngx.exit(403)
                    ngx.print("should never appear")
                end
            "#;
            let (status, _h, body) = run_one(src, get("/secret")).await;
            assert_eq!(status, StatusCode::FORBIDDEN);
            assert!(body.is_empty(), "post-exit body leaked: {body:?}");
        })
        .await;
}

#[tokio::test]
async fn ngx_req_reads_method_and_uri() {
    LocalSet::new()
        .run_until(async {
            let src = r#"
                return function()
                    ngx.print(ngx.req.get_method(), " ", ngx.req.get_path())
                end
            "#;
            let (_s, _h, body) = run_one(src, get("/users/42?page=3")).await;
            assert_eq!(body.as_ref(), b"GET /users/42");
        })
        .await;
}

#[tokio::test]
async fn say_defaults_to_text_plain_print_does_not() {
    LocalSet::new()
        .run_until(async {
            // ngx.say -> text/plain default; ngx.print alone -> no content-type.
            let say_src = "return function() ngx.say('hi') end";
            let print_src = "return function() ngx.print('hi') end";
            let (_, h_say, _) = run_one(say_src, get("/")).await;
            let (_, h_print, _) = run_one(print_src, get("/")).await;
            assert!(h_say.iter().any(|(n, _)| n == "content-type"));
            assert!(!h_print.iter().any(|(n, _)| n == "content-type"));
        })
        .await;
}

/// The Q13 killer test (logical): two concurrent requests interleave and each
/// resolves to its OWN status/body, proving the coroutine-local binding.
#[tokio::test]
async fn concurrent_requests_keep_distinct_context() {
    LocalSet::new()
        .run_until(async {
            let src = r#"
                return function()
                    local m = ngx.req.get_headers()["x-tag"]
                    ngx.sleep(15)
                    ngx.status = tonumber(m)
                    ngx.say("done ", m)
                end
            "#;
            let p = Pipeline::new(src).expect("pipeline");
            let req_a = Request::builder()
                .header("x-tag", "201")
                .uri("/a")
                .body(())
                .unwrap();
            let req_b = Request::builder()
                .header("x-tag", "418")
                .uri("/b")
                .body(())
                .unwrap();
            let (oa, ob) = join2(p.serve_request(req_a), p.serve_request(req_b)).await;
            let ra = build_response(oa);
            let rb = build_response(ob);
            assert_eq!(ra.status(), StatusCode::CREATED);
            assert_eq!(rb.status(), StatusCode::IM_A_TEAPOT);
            assert_eq!(ra.body().as_ref(), b"done 201\n");
            assert_eq!(rb.body().as_ref(), b"done 418\n");
        })
        .await;
}

// ---- real-socket test (the T5.2a acceptance) ----

/// Minimal HTTP/1.1 client: send a request, return (status, header map, body).
async fn http_request(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    extra_headers: &[(&str, &str)],
) -> (u16, Vec<(String, String)>, Vec<u8>) {
    let mut stream = TcpStream::connect(addr).await.expect("connect");
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\n");
    for (k, v) in extra_headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("connection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).await.expect("write");
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
            // body begins after this blank line; compute byte offset.
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

#[tokio::test]
async fn real_client_observes_content_phase_over_tcp() {
    LocalSet::new()
        .run_until(async {
            let src = r#"
                return function()
                    ngx.status = 201
                    ngx.header["X-Served-By"] = "init-pro-router"
                    ngx.say("hello over the wire")
                end
            "#;
            let pipeline = Pipeline::new(src).expect("pipeline");
            let (addr, listener) = router::ephemeral_listener().expect("listener");
            let (_tx, rx) = tokio::sync::oneshot::channel::<()>();
            let server = tokio::task::spawn_local(router::serve(pipeline, listener, async move {
                let _ = rx.await;
            }));
            // serve the first request, then shut down.
            let (status, headers, body) = http_request(addr, "GET", "/anything", &[]).await;
            let _ = _tx.send(());
            let _ = server.await;

            assert_eq!(status, 201);
            assert_eq!(body, b"hello over the wire\n");
            assert_eq!(
                headers
                    .iter()
                    .find(|(n, _)| n == "x-served-by")
                    .map(|(_, v)| v.clone()),
                Some("init-pro-router".to_owned())
            );
        })
        .await;
}

#[tokio::test]
async fn real_client_concurrent_requests_stay_distinct_over_tcp() {
    LocalSet::new()
        .run_until(async {
            // Echo the x-tag header into status + body; two concurrent
            // requests must each see their own tag.
            let src = r#"
                return function()
                    local tag = ngx.req.get_headers()["x-tag"]
                    ngx.sleep(15)
                    ngx.status = tonumber(tag)
                    ngx.print("tag=", tag)
                end
            "#;
            let pipeline = Pipeline::new(src).expect("pipeline");
            let (addr, listener) = router::ephemeral_listener().expect("listener");
            let (_tx, rx) = tokio::sync::oneshot::channel::<()>();
            let server = tokio::task::spawn_local(router::serve(pipeline, listener, async move {
                let _ = rx.await;
            }));

            let a = tokio::task::spawn_local(http_request(addr, "GET", "/a", &[("x-tag", "201")]));
            let b = tokio::task::spawn_local(http_request(addr, "GET", "/b", &[("x-tag", "503")]));
            let (sa, _ha, ba) = a.await.unwrap();
            let (sb, _hb, bb) = b.await.unwrap();
            let _ = _tx.send(());
            let _ = server.await;

            assert_eq!(sa, 201);
            assert_eq!(ba, b"tag=201");
            assert_eq!(sb, 503);
            assert_eq!(bb, b"tag=503");
        })
        .await;
}
