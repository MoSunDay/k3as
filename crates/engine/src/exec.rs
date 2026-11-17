//! Exec orchestration (T4.12, Q50): one in-flight `nsrun::exec_into`
//! relay per engine. Output rides the connection's kinds 2/3 untagged
//! (single in-flight makes tags unnecessary); the JSON delimiters are
//! `ExecStarted`/`ExecExited`.

use std::io;
use std::os::unix::net::UnixStream;
use std::time::Duration;

use engine_proto::{encode_json, Event, FrameKind};
use nsrun::{ExecProc, ExecSpec, Output};

use crate::wire::write_polled;

/// Per-pump slice: short so container output interleaves promptly.
const EXEC_SLICE: Duration = Duration::from_millis(20);
/// Monotonic exec ids: `exec-<n>`; the kubelet only matches them back.
pub(crate) fn next_exec_id(seq: &mut u64) -> String {
    *seq += 1;
    format!("exec-{seq}")
}

/// Starts the relay for a container `child_pid`.
pub(crate) fn start(
    child_pid: u32,
    argv: Vec<String>,
    env: Vec<String>,
    workdir: String,
) -> io::Result<ExecProc> {
    // `pid_t` is i32 on linux; the engine never names libc itself.
    let mut spec = ExecSpec::new(child_pid as i32, argv);
    spec.env = env;
    spec.cwd = workdir.into();
    nsrun::exec_into(&spec)
}

/// What a pump pass produced.
pub(crate) enum Progress {
    /// Nothing happened this slice.
    Idle,
    /// The exec finished; the `ExecExited` frame was already sent.
    Done,
}

/// Pumps one slice of the running exec: raw output frames first, then the
/// `ExecExited` delimiter when the relay reaped. `Err` means the peer
/// stopped reading (the session drops the exec).
pub(crate) fn pump(
    exec_id: &str,
    proc: &mut ExecProc,
    stream: &mut UnixStream,
) -> io::Result<Progress> {
    loop {
        match proc.next_event(EXEC_SLICE) {
            Ok(Some(Output::Stdout(bytes))) => {
                write_polled(stream, &raw_frame(FrameKind::Stdout, &bytes))?;
            }
            Ok(Some(Output::Stderr(bytes))) => {
                write_polled(stream, &raw_frame(FrameKind::Stderr, &bytes))?;
            }
            Ok(Some(Output::Exited(code))) => {
                let event = Event::ExecExited {
                    exec_id: exec_id.to_string(),
                    code,
                };
                send_event(stream, &event)?;
                return Ok(Progress::Done);
            }
            Ok(None) => return Ok(Progress::Idle),
            Err(_) => {
                // A broken relay is still a finished exec: report it.
                let event = Event::ExecExited {
                    exec_id: exec_id.to_string(),
                    code: 125,
                };
                send_event(stream, &event)?;
                return Ok(Progress::Done);
            }
        }
    }
}

fn raw_frame(kind: FrameKind, payload: &[u8]) -> Vec<u8> {
    let mut v = (payload.len() as u32).to_le_bytes().to_vec();
    v.push(kind.as_u8());
    v.extend_from_slice(payload);
    v
}

fn send_event(stream: &mut UnixStream, event: &Event) -> io::Result<()> {
    let payload = encode_json(event).map_err(|e| io::Error::other(format!("encode: {e}")))?;
    write_polled(stream, &raw_frame(FrameKind::Json, &payload))
}

#[cfg(test)]
mod tests {
    #[test]
    fn exec_ids_are_monotonic() {
        let mut seq = 0u64;
        assert_eq!(super::next_exec_id(&mut seq), "exec-1");
        assert_eq!(super::next_exec_id(&mut seq), "exec-2");
    }
}
