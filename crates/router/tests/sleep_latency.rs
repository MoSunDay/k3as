//! `ngx.sleep` round-trip latency baseline (T5.1).
//!
//! Measures the coroutine<->async bridge overhead for a single sleep and
//! asserts it lands in a sane window — proving there is no gross cost to
//! parking/resuming a Lua coroutine across a Rust future. The number printed
//! is the v1 latency baseline referenced by the plan (cosocket microbench is
//! T5.2, which needs the HTTP pipeline).

use std::time::{Duration, Instant};

use mlua::Function;
use router::worker_vm;
use tokio::task::LocalSet;

#[tokio::test]
async fn ngx_sleep_latency_is_in_tolerance() {
    LocalSet::new()
        .run_until(async {
            let lua = worker_vm().expect("worker vm");
            let f = lua
                .load("return function() ngx.sleep(10) end")
                .eval::<Function>()
                .expect("lua function");

            // Warm the timer wheel / coroutine path once.
            f.call_async::<()>(()).await.expect("warmup call");

            let start = Instant::now();
            f.call_async::<()>(()).await.expect("timed call");
            let elapsed = start.elapsed();

            println!("ngx.sleep(10ms) round-trip latency: {elapsed:?}");
            assert!(elapsed >= Duration::from_millis(8), "too fast: {elapsed:?}");
            assert!(
                elapsed <= Duration::from_millis(60),
                "too slow: {elapsed:?}"
            );
        })
        .await;
}
