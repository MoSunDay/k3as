//! Kill-criterion (T5.1 ★成败判定★).
//!
//! Proves a Lua coroutine **yields at a Rust `await` point**, letting another
//! coroutine run concurrently on the same worker VM — i.e. the
//! coroutine<->async bridge is real and non-blocking, not a fake that parks
//! the whole worker thread. This is the single highest-risk unknown of Q4.
//!
//! VM model (ADR Q12): one worker-wide LuaJIT VM, per-coroutine Lua threads,
//! driven on a single-thread `tokio::task::LocalSet` — openresty's per-worker
//! coroutine scheduler, reproduced in Rust.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use mlua::Function;
use router::worker_vm;
use tokio::task::LocalSet;

/// Ordered log of `(label, elapsed-since-vm-start)` recorded from Lua.
type EventLog = Arc<Mutex<Vec<(String, Duration)>>>;

/// Concurrently poll a batch of `!Send` coroutine futures (they borrow the VM)
/// to completion, propagating the first error. A dependency-free analogue of
/// `futures::future::join_all` for the single-thread case.
async fn join_all<Fut>(futs: Vec<Fut>) -> Result<(), mlua::Error>
where
    Fut: Future<Output = Result<(), mlua::Error>>,
{
    let mut slots: Vec<Option<Pin<Box<Fut>>>> =
        futs.into_iter().map(|f| Some(Box::pin(f))).collect();
    std::future::poll_fn(move |cx: &mut Context<'_>| {
        let mut pending = false;
        for slot in slots.iter_mut() {
            let Some(fut) = slot.as_mut() else { continue };
            match fut.as_mut().poll(cx) {
                Poll::Ready(Ok(())) => *slot = None,
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => pending = true,
            }
        }
        if pending {
            Poll::Pending
        } else {
            Poll::Ready(Ok(()))
        }
    })
    .await
}

/// Build a worker VM with a `log(label)` callback that records `(label,
/// elapsed-since-vm-start)` into a shared, ordered vector. Returns the log and
/// the VM.
fn vm_with_log() -> (mlua::Lua, EventLog, Instant) {
    let start = Instant::now();
    let log = Arc::new(Mutex::new(Vec::new()));
    let lua = worker_vm().expect("worker vm");
    let record = lua
        .create_function({
            let log = log.clone();
            move |_, label: String| {
                log.lock().unwrap().push((label, start.elapsed()));
                Ok(())
            }
        })
        .expect("log fn");
    lua.globals().set("log", record).expect("set log");
    (lua, log, start)
}

/// Two coroutines driven concurrently: A sleeps long, B sleeps short. If the
/// bridge is real, B starts (and finishes) **while A is parked** at its sleep;
/// if it were fake/blocking, A would monopolise the thread and finish before B
/// is ever polled. The ordering assertion is the discriminator; the wall-clock
/// assertion is a secondary guard.
#[tokio::test]
async fn coroutine_yields_on_async_sleep() {
    LocalSet::new()
        .run_until(async {
            let (lua, log, _) = vm_with_log();

            let fa = lua
                .load("return function() log('A_start') ngx.sleep(50) log('A_end') end")
                .eval::<Function>()
                .expect("fn A");
            let fb = lua
                .load("return function() log('B_start') ngx.sleep(5) log('B_end') end")
                .eval::<Function>()
                .expect("fn B");

            let wall = Instant::now();
            let (ra, rb) = tokio::join!(fa.call_async::<()>(()), fb.call_async::<()>(()));
            ra.expect("coroutine A");
            rb.expect("coroutine B");
            let total = wall.elapsed();

            let entries = log.lock().unwrap();
            let ts = |label: &str| -> Duration {
                entries
                    .iter()
                    .find(|(l, _)| l == label)
                    .unwrap_or_else(|| panic!("missing log entry {label}"))
                    .1
            };
            let a_end = ts("A_end");
            let b_start = ts("B_start");
            let b_end = ts("B_end");

            // === KILL-CRITERION ===
            // Real bridge => B runs inside A's sleep window (b_start < a_end,
            // b_end < a_end). A blocking/fake bridge => fully serial order
            // A_start < A_end < B_start, i.e. b_start >= a_end. Fail => Q4
            // escalate (plan R1).
            assert!(
                b_start < a_end,
                "BRIDGE FAKE (blocking): B_start ({b_start:?}) must precede A_end ({a_end:?})",
            );
            assert!(b_end < a_end, "B must finish inside A's sleep window");

            // Wall clock ~ max(50,5)=50ms (concurrent), never the serial sum.
            assert!(total >= Duration::from_millis(45), "underslept: {total:?}");
            assert!(
                total < Duration::from_millis(200),
                "overslept (serial?): {total:?}"
            );

            let order: Vec<&str> = entries.iter().map(|(l, _)| l.as_str()).collect();
            println!(
                "PASS concurrency: order={order:?}; total wall={total:?} (concurrent, not serial)"
            );
        })
        .await;
}

/// Scale test: 10 coroutines each `ngx.sleep(20)` complete in ~max (one sleep
/// period), not ~sum (10 periods). Proves the bridge scales, not just for two.
#[tokio::test]
async fn many_coroutines_scale_to_max_not_sum() {
    LocalSet::new()
        .run_until(async {
            let lua = worker_vm().expect("worker vm");
            let code = "return function() ngx.sleep(20) end";
            let funcs: Vec<Function> = (0..10_u8)
                .map(|_| lua.load(code).eval::<Function>().expect("fn"))
                .collect();
            let futs: Vec<_> = funcs.iter().map(|f| f.call_async::<()>(())).collect();

            let start = Instant::now();
            join_all(futs).await.expect("all coroutines");
            let elapsed = start.elapsed();

            // 10x 20ms concurrent => ~20ms; serial sum would be ~200ms.
            assert!(
                elapsed < Duration::from_millis(100),
                "scaling: {elapsed:?} should be ~max(20ms), not ~sum(200ms)",
            );
            println!("PASS scaling: 10 coroutines x ngx.sleep(20ms) = {elapsed:?} total");
        })
        .await;
}
