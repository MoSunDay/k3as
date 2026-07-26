//! T5.2b acceptance: cosocket (`ngx.socket.tcp`) echo over a real TCP server,
//! driven from a Lua coroutine. Measures the connect/send/receive round-trip
//! latency as the v1 cosocket baseline (the sibling of T5.1's `sleep_latency`).

use std::time::{Duration, Instant};

use mlua::Function;
use router::worker_vm;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::task::LocalSet;

/// A minimal echo server: every byte written is written straight back.
async fn echo_server(listener: TcpListener, shutdown: tokio::sync::oneshot::Receiver<()>) {
    let mut shutdown = Box::pin(shutdown);
    loop {
        tokio::select! {
            biased;
            _ = &mut shutdown => break,
            res = listener.accept() => {
                let (mut sock, _) = match res { Ok(s) => s, Err(_) => continue };
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    loop {
                        match sock.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => { if sock.write_all(&buf[..n]).await.is_err() { break; } }
                        }
                    }
                });
            }
        }
    }
}

#[tokio::test]
async fn cosocket_echo_roundtrip_matches() {
    LocalSet::new()
        .run_until(async {
            let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
            let addr = listener.local_addr().expect("addr");
            let (tx, rx) = tokio::sync::oneshot::channel();
            let _srv = tokio::spawn(echo_server(listener, rx));

            let lua = worker_vm().expect("worker vm");
            let f = lua
                .load(
                    r#"
                    return function(host, port)
                        local sock = ngx.socket.tcp()
                        sock:settimeout(2000)
                        assert(sock:connect(host, port) == 1, "connect")
                        local payload = string.rep("x", 128)
                        local sent = sock:send(payload)
                        assert(sent == #payload, "send bytes")
                        local got = sock:receive(#payload)
                        sock:close()
                        return got
                    end
                    "#,
                )
                .eval::<Function>()
                .expect("lua function");

            let got: mlua::LuaString = f
                .call_async::<mlua::LuaString>(("127.0.0.1", addr.port()))
                .await
                .expect("call_async");
            assert_eq!(got.as_bytes(), &vec![b'x'; 128][..]);
            let _ = tx.send(());
        })
        .await;
}

#[tokio::test]
async fn cosocket_receive_line_mode() {
    LocalSet::new()
        .run_until(async {
            let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
            let addr = listener.local_addr().expect("addr");
            let (tx, rx) = tokio::sync::oneshot::channel();
            let _srv = tokio::spawn(echo_server(listener, rx));

            let lua = worker_vm().expect("worker vm");
            let f = lua
                .load(
                    r#"
                    return function(host, port)
                        local sock = ngx.socket.tcp()
                        sock:settimeout(2000)
                        sock:connect(host, port)
                        sock:send("hello\nworld\n")
                        local a = sock:receive()
                        local b = sock:receive("*l")
                        sock:close()
                        return a, b
                    end
                    "#,
                )
                .eval::<Function>()
                .expect("lua function");

            let (a, b): (mlua::LuaString, mlua::LuaString) = f
                .call_async::<(mlua::LuaString, mlua::LuaString)>(("127.0.0.1", addr.port()))
                .await
                .expect("call_async");
            assert_eq!(a.as_bytes(), b"hello\n");
            assert_eq!(b.as_bytes(), b"world\n");
            let _ = tx.send(());
        })
        .await;
}

/// The v1 cosocket latency baseline (mirrors T5.1's sleep_latency).
#[tokio::test]
async fn cosocket_echo_latency_baseline() {
    LocalSet::new()
        .run_until(async {
            let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
            let addr = listener.local_addr().expect("addr");
            let (tx, rx) = tokio::sync::oneshot::channel();
            let _srv = tokio::spawn(echo_server(listener, rx));

            let lua = worker_vm().expect("worker vm");
            let f = lua
                .load(
                    r#"
                    return function(host, port, payload, n)
                        local sock = ngx.socket.tcp()
                        sock:settimeout(2000)
                        sock:connect(host, port)
                        for _ = 1, n do
                            sock:send(payload)
                            sock:receive(#payload)
                        end
                        sock:close()
                    end
                    "#,
                )
                .eval::<Function>()
                .expect("lua function");

            // Warm up.
            f.call_async::<()>(("127.0.0.1", addr.port(), "warmup", 1i64))
                .await
                .expect("warmup");

            const N: i64 = 200;
            const SZ: i64 = 64;
            let start = Instant::now();
            f.call_async::<()>(("127.0.0.1", addr.port(), "x".repeat(SZ as usize), N))
                .await
                .expect("timed run");
            let elapsed = start.elapsed();

            let per_rt = elapsed / N as u32;
            println!(
                "cosocket echo: {N} round-trips x {SZ}B = {elapsed:?} total, ~{per_rt:?}/rt"
            );
            // Sanity: each localhost round-trip should be well under 10ms.
            assert!(
                per_rt < Duration::from_millis(10),
                "cosocket round-trip too slow: {per_rt:?}"
            );
            let _ = tx.send(());
        })
        .await;
}
