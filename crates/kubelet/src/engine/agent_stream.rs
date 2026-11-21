//! T7.7 (Q63) hardened-attempt engine stream: the verdict event loop over
//! one attempt's `RunStream` — bounded by BOTH the exec budget and the
//! kill switch (the agent_exec::verdict structure), tail-capping stderr
//! for session-id parity. Split from `agent_exec_sandbox.rs` to respect
//! the 400-line file cap.

use std::time::Duration;

use engine_client::{RunStream, StreamEvent};
use tokio::sync::watch;

/// Hard cap on kept runner stderr (the session-id tail is all we need).
const STDERR_CAP: usize = 64 * 1024;

/// How one sandboxed attempt ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SandboxVerdict {
    Exited(i32),
    Error(String),
    Timeout,
    Cancelled,
}

/// The event loop: both the exec budget and the kill switch bound the
/// wait (agent_exec::verdict structure over a RunStream). Stderr is kept
/// tail-capped for `session_id_from_stderr` parity; stdout is counted
/// and discarded.
pub(crate) async fn wait_verdict(
    stream: &mut RunStream,
    id: &str,
    exec_timeout: Duration,
    keep_going: &mut watch::Receiver<bool>,
) -> (SandboxVerdict, String) {
    let deadline = tokio::time::Instant::now() + exec_timeout;
    let mut stderr = String::new();
    loop {
        let verdict = {
            let next = stream.next();
            tokio::pin!(next);
            tokio::select! {
                ev = &mut next => match ev {
                    Ok(Some(StreamEvent::Started { pid })) => {
                        tracing::info!(target: "init-pro", attempt = %id, pid, "runner started (Q63)");
                        None
                    }
                    Ok(Some(StreamEvent::Stdout(_))) => None, // discarded
                    Ok(Some(StreamEvent::Stderr(b))) => {
                        push_tail(&mut stderr, &b);
                        None
                    }
                    Ok(Some(StreamEvent::Exited { code })) => Some(SandboxVerdict::Exited(code)),
                    Ok(Some(StreamEvent::Error(message))) => Some(SandboxVerdict::Error(message)),
                    // Exec frames never ride this stream; Bye/None after
                    // Exited is the engine's clean goodbye.
                    Ok(Some(_) | None) => Some(SandboxVerdict::Exited(-1)),
                    Err(e) => Some(SandboxVerdict::Error(e.to_string())),
                },
                res = keep_going.changed() => match res {
                    Ok(()) if !*keep_going.borrow_and_update() => Some(SandboxVerdict::Cancelled),
                    Ok(()) => None,
                    // All senders gone: nobody can cancel; wait it out.
                    Err(_) => None,
                },
                _ = tokio::time::sleep_until(deadline) => Some(SandboxVerdict::Timeout),
            }
        };
        if let Some(v) = verdict {
            return (v, stderr);
        }
    }
}

/// Keep only the STDERR_CAP tail (the session id prints at completion).
fn push_tail(buf: &mut String, bytes: &[u8]) {
    buf.push_str(&String::from_utf8_lossy(bytes));
    if buf.len() > STDERR_CAP {
        let mut cut = buf.len() - STDERR_CAP;
        while !buf.is_char_boundary(cut) {
            cut += 1;
        }
        buf.drain(..cut);
    }
}
