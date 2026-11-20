//! Client sessions (T4.11/T4.12, Q48-Q50): handshake, then requests on
//! ONE connection — a running container is multiplexed, not exclusive.
//!
//! The whole session reads through [`crate::wire::FrameBuffer`] on a
//! non-blocking socket: mid-container `Signal`/`Exec`/`Ping`/`Shutdown`
//! are served in the same loop that streams container output (Q49/Q50),
//! and a client that vanishes mid-run gets its container killed (Q49:
//! agent restart means containers die). Pause engines (Q49) answer
//! `Ping` with the pause pid and turn `Signal` into a sandbox stop.
//! The engine owns a single container, so pending peers still get the
//! `engine busy` notice, never a session.

use std::io;
use std::os::unix::net::{UnixListener, UnixStream};
use std::time::{Duration, Instant};

use engine_proto::{decode_json, encode_json, ContainerSpec, Event, Frame, FrameKind, Request};
use nsrun::{Child, Output};

use crate::exec;
use crate::logs::LogWriter;
use crate::server::SHUTDOWN;
use crate::wire::{write_polled, FrameBuffer, PeerInput};

/// Handshake budget: a peer that connects must say something by now.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
/// Container event slice; also the busy-poll period.
const EVENT_SLICE: Duration = Duration::from_millis(200);
/// Slice while an exec is in flight so its output interleaves promptly.
const EXEC_ACTIVE_SLICE: Duration = Duration::from_millis(20);
/// Idle poll of the accept-free session loop (no container, no exec).
const IDLE_POLL: Duration = Duration::from_millis(20);
/// Pending peers answered per slice (a burst, then back to the container).
const BUSY_BURST: usize = 8;
/// SIGKILL budget before an unreaped container is left to `Drop`.
const KILL_GRACE: Duration = Duration::from_secs(5);
/// How long a signalled pause sandbox may take to die before SIGKILL.
const PAUSE_STOP_GRACE: Duration = Duration::from_secs(10);
/// Bounded wait for the exec relay's container-ns pid report.
const EXEC_PID_WAIT: Duration = Duration::from_millis(500);

/// What the server should do after a session: keep accepting, or exit 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Continue,
    Exit,
}

/// Serve-level defaults applied to every `Run` (T4.12: `--netns`,
/// `--log-path`, `--mount` on `init-pro engine serve`).
pub(crate) struct SessionCtx {
    pub netns_default: Option<String>,
    pub log_default: Option<std::path::PathBuf>,
    pub mounts_default: Vec<engine_proto::MountSpec>,
}

/// Serves one connection. `pause` is the sandbox child of a pause engine
/// (Q49); normal engines pass `None`. Returns [`Outcome::Exit`] when the
/// peer asked for shutdown, a sandbox stopped, or a shutdown signal
/// arrived mid-container.
pub fn handle(
    mut stream: UnixStream,
    listener: &UnixListener,
    ctx: &SessionCtx,
    pause: &mut Option<Child>,
) -> io::Result<Outcome> {
    stream.set_nonblocking(true)?;
    let mut fb = FrameBuffer::new();
    let mut greeted = false;
    loop {
        let deadline = greeted.then(|| Instant::now() + HANDSHAKE_TIMEOUT);
        // Wait for one frame (or EOF), bounded by the handshake deadline
        // only until the peer has proven it speaks.
        let input = poll_bounded(&mut fb, &mut stream, deadline)?;
        let frame = match input {
            PeerInput::Eof => return Ok(Outcome::Continue),
            PeerInput::Idle => {
                if pause.is_some() {
                    if let Some(out) = pause_idle(pause) {
                        return Ok(out);
                    }
                }
                continue;
            }
            PeerInput::Frame(f) => f,
        };
        greeted = true;
        if frame.kind == FrameKind::Stdin {
            send_error(&mut stream, "stdin frames are refused (Q50)")?;
            continue;
        }
        if frame.kind != FrameKind::Json {
            send_error(&mut stream, "unexpected binary frame on the request path")?;
            continue;
        }
        let request: Request = match decode_json(&frame.payload) {
            Ok(r) => r,
            Err(e) => {
                send_error(&mut stream, &format!("bad request JSON: {e}"))?;
                continue;
            }
        };
        match request {
            Request::Ping => {
                let pid = pause.as_ref().map(|c| c.pid());
                send_event(&mut stream, &Event::Pong { pid })?;
            }
            Request::Shutdown => {
                if let Some(child) = pause.as_mut() {
                    // A pause engine's Shutdown is a sandbox remove (Q49):
                    // kill the child, answer Bye, exit.
                    child.kill();
                    let _ = child.wait_timeout(KILL_GRACE);
                    child.cleanup();
                    *pause = None;
                }
                send_event(&mut stream, &Event::Bye)?;
                return Ok(Outcome::Exit);
            }
            Request::Signal { id, signo } => {
                if pause.is_some() {
                    if let Some(out) = pause_signal(&mut stream, pause, &id, signo)? {
                        return Ok(out);
                    }
                } else {
                    send_error(&mut stream, "no container is running on this engine")?;
                }
            }
            Request::Exec { .. } => {
                send_error(&mut stream, "no container is running on this engine")?;
            }
            Request::Run { mut spec } => {
                if pause.is_some() {
                    send_error(&mut stream, "a pause engine cannot run containers")?;
                    continue;
                }
                apply_defaults(&mut spec, ctx);
                match run_one(&mut stream, spec, listener, &mut fb)? {
                    Outcome::Continue => {}
                    Outcome::Exit => return Ok(Outcome::Exit),
                }
            }
        }
    }
}

/// Polls once, sleeping in small slices until something happens or the
/// (optional) deadline expires. A deadline miss on an ungreeted peer is a
/// clean `Continue`, matching the old read-timeout behavior.
fn poll_bounded(
    fb: &mut FrameBuffer,
    stream: &mut UnixStream,
    deadline: Option<Instant>,
) -> io::Result<PeerInput> {
    loop {
        match fb.poll(stream)? {
            PeerInput::Idle => {
                let Some(dl) = deadline else {
                    std::thread::sleep(IDLE_POLL);
                    continue;
                };
                if Instant::now() >= dl {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "peer never sent a request",
                    ));
                }
                std::thread::sleep(IDLE_POLL);
            }
            other => return Ok(other),
        }
    }
}

/// Serve-level defaults: explicit spec values always win (Q49/Q50).
fn apply_defaults(spec: &mut ContainerSpec, ctx: &SessionCtx) {
    // T4.15 (Q60): a host-root container never joins the serve-level
    // netns default either — the host network stays.
    if !spec.host_root && spec.netns.is_none() && !spec.new_net {
        spec.netns = ctx.netns_default.clone();
    }
    if spec.log_path.is_none() {
        spec.log_path = ctx
            .log_default
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned());
    }
    spec.mounts.extend(ctx.mounts_default.iter().cloned());
}

/// A pause engine with no client: watch the sandbox child so a signalled
/// pause still ends the engine promptly.
fn pause_idle(pause: &mut Option<Child>) -> Option<Outcome> {
    let child = pause.as_mut()?;
    match child.next_event(IDLE_POLL) {
        Ok(Some(Output::Exited(_))) => {
            child.cleanup();
            *pause = None;
            Some(Outcome::Exit)
        }
        _ => None,
    }
}

/// A pause engine with a client: `Ping`/`Signal`/`Shutdown` semantics per
/// Q49. `Signal` is a sandbox stop: signal, bounded wait, `Exited`, exit.
fn pause_signal(
    stream: &mut UnixStream,
    pause: &mut Option<Child>,
    id: &str,
    signo: i32,
) -> io::Result<Option<Outcome>> {
    let Some(child) = pause.as_mut() else {
        send_error(stream, "sandbox already stopped")?;
        return Ok(Some(Outcome::Exit));
    };
    child.signal(signo);
    match child.wait_timeout(PAUSE_STOP_GRACE) {
        Ok(code) => {
            child.cleanup();
            *pause = None;
            send_event(
                stream,
                &Event::Exited {
                    id: id.into(),
                    code,
                },
            )?;
            send_event(stream, &Event::Bye)?;
            Ok(Some(Outcome::Exit))
        }
        Err(_) => {
            // Still alive after the grace budget: report and keep serving.
            send_error(stream, "sandbox did not exit after the signal grace")?;
            Ok(None)
        }
    }
}

fn run_one(
    stream: &mut UnixStream,
    spec: ContainerSpec,
    listener: &UnixListener,
    fb: &mut FrameBuffer,
) -> io::Result<Outcome> {
    let id = spec.id.clone();
    // T4.13 M4 (Q53): execs with an empty env inherit the CONTAINER's
    // merged env (image env + pod env) — k8s exec semantics.
    let container_env = spec.env.clone();
    let mut log = match spec
        .log_path
        .as_deref()
        .map(std::path::Path::new)
        .map(LogWriter::open)
    {
        Some(Ok(l)) => Some(l),
        Some(Err(e)) => {
            send_error(stream, &format!("open log file: {e}"))?;
            return Ok(Outcome::Continue);
        }
        None => None,
    };
    let run = match crate::convert::to_run_spec(&spec) {
        Ok(r) => r,
        Err(e) => {
            send_error(stream, &e)?;
            return Ok(Outcome::Continue);
        }
    };
    let mut child = match nsrun::spawn(&run) {
        Ok(c) => c,
        Err(e) => {
            send_error(stream, &format!("spawn {}: {e}", spec.id))?;
            return Ok(Outcome::Continue);
        }
    };
    send_event(
        stream,
        &Event::Started {
            id: id.clone(),
            pid: child.pid(),
        },
    )?;
    let mut live_exec: Option<(String, nsrun::ExecProc)> = None;
    let mut exec_seq: u64 = 0;
    loop {
        if nsrun::shutdown_requested(&SHUTDOWN) {
            finish_exec(stream, &mut live_exec);
            stop(&mut child);
            let _ = log.as_mut().map(|l| l.finish());
            notify(stream, "engine shutting down; container killed");
            return Ok(Outcome::Exit);
        }
        // Client first: signals and execs must not wait behind output.
        match fb.poll(stream)? {
            PeerInput::Eof => {
                // Q49: the kubelet vanished mid-run; the container goes
                // with it. Nobody is left to notify.
                finish_exec(stream, &mut live_exec);
                stop(&mut child);
                let _ = log.as_mut().map(|l| l.finish());
                return Ok(Outcome::Continue);
            }
            PeerInput::Frame(frame) => {
                if let Some(Outcome::Exit) = peer_request(
                    stream,
                    frame,
                    &id,
                    &mut child,
                    &mut live_exec,
                    &mut exec_seq,
                    &container_env,
                )? {
                    let _ = log.as_mut().map(|l| l.finish());
                    return Ok(Outcome::Exit);
                }
            }
            PeerInput::Idle => reply_busy(listener),
        }
        if let Some((exec_id, proc)) = live_exec.as_mut() {
            if let exec::Progress::Done = exec::pump(exec_id, proc, stream)? {
                live_exec = None;
            }
        }
        let slice = if live_exec.is_some() {
            EXEC_ACTIVE_SLICE
        } else {
            EVENT_SLICE
        };
        match child.next_event(slice) {
            Ok(Some(Output::Stdout(bytes))) => {
                write_output(stream, FrameKind::Stdout, &bytes)?;
                if let Some(l) = log.as_mut() {
                    l.chunk(false, &bytes)?;
                }
            }
            Ok(Some(Output::Stderr(bytes))) => {
                write_output(stream, FrameKind::Stderr, &bytes)?;
                if let Some(l) = log.as_mut() {
                    l.chunk(true, &bytes)?;
                }
            }
            Ok(Some(Output::Exited(code))) => {
                finish_exec(stream, &mut live_exec);
                if let Some(l) = log.as_mut() {
                    l.finish()?;
                }
                child.cleanup();
                send_event(stream, &Event::Exited { id, code })?;
                return Ok(Outcome::Continue);
            }
            Ok(None) => {}
            Err(e) => {
                finish_exec(stream, &mut live_exec);
                send_error(stream, &format!("container {id}: {e}"))?;
                stop(&mut child);
                let _ = log.as_mut().map(|l| l.finish());
                return Ok(Outcome::Continue);
            }
        }
    }
}

/// One mid-run client frame: `Signal`/`Exec`/`Ping`/`Shutdown` (Q49/Q50).
/// `Some(Outcome::Exit)` only for a peer-requested shutdown.
fn peer_request(
    stream: &mut UnixStream,
    frame: Frame,
    id: &str,
    child: &mut Child,
    live_exec: &mut Option<(String, nsrun::ExecProc)>,
    exec_seq: &mut u64,
    container_env: &[String],
) -> io::Result<Option<Outcome>> {
    if frame.kind == FrameKind::Stdin {
        send_error(stream, "stdin frames are refused (Q50)")?;
        return Ok(None);
    }
    if frame.kind != FrameKind::Json {
        send_error(stream, "unexpected binary frame on the request path")?;
        return Ok(None);
    }
    let request: Request = match decode_json(&frame.payload) {
        Ok(r) => r,
        Err(e) => {
            send_error(stream, &format!("bad request JSON: {e}"))?;
            return Ok(None);
        }
    };
    match request {
        Request::Signal { id: sid, signo } => {
            if sid == id {
                child.signal(signo);
            } else {
                send_error(stream, &format!("signal: unknown container {sid}"))?;
            }
        }
        Request::Ping => send_event(stream, &Event::Pong { pid: None })?,
        Request::Exec {
            id: eid,
            argv,
            env,
            workdir,
        } => {
            if eid != id {
                send_error(stream, &format!("exec: unknown container {eid}"))?;
            } else if live_exec.is_some() {
                send_error(stream, "exec busy: one in-flight exec per engine (Q50)")?;
            } else {
                // T4.13 M4 (Q53): an empty exec env means "the container's
                // merged env" (the kubelet pump sends none).
                let env = if env.is_empty() {
                    container_env.to_vec()
                } else {
                    env
                };
                match exec::start(child.pid(), argv, env, workdir) {
                    Ok(mut proc) => {
                        let exec_id = exec::next_exec_id(exec_seq);
                        // The relay writes its pid report before execve, so
                        // a short bounded wait almost always has it. Any
                        // output seen while waiting is kept and replayed
                        // after ExecStarted so ordering holds.
                        let mut pending: Vec<Output> = Vec::new();
                        let waited = Instant::now() + EXEC_PID_WAIT;
                        while proc.container_pid().is_none() && Instant::now() < waited {
                            if let Some(out) = proc.next_event(Duration::from_millis(10))? {
                                pending.push(out);
                            }
                        }
                        let pid = proc.container_pid().unwrap_or(0);
                        send_event(
                            stream,
                            &Event::ExecStarted {
                                exec_id: exec_id.clone(),
                                pid,
                            },
                        )?;
                        for out in pending {
                            replay_exec_output(stream, out)?;
                        }
                        *live_exec = Some((exec_id, proc));
                    }
                    Err(e) => send_error(stream, &format!("exec: {e}"))?,
                }
            }
        }
        Request::Run { .. } => {
            send_error(stream, "engine busy: one container at a time")?;
        }
        Request::Shutdown => {
            finish_exec(stream, live_exec);
            stop(child);
            send_event(
                stream,
                &Event::Exited {
                    id: id.into(),
                    code: 143,
                },
            )?;
            send_event(stream, &Event::Bye)?;
            return Ok(Some(Outcome::Exit));
        }
    }
    Ok(None)
}

/// Replays output collected while waiting for the exec pid report.
fn replay_exec_output(stream: &mut UnixStream, out: Output) -> io::Result<()> {
    match out {
        Output::Stdout(b) => write_output(stream, FrameKind::Stdout, &b),
        Output::Stderr(b) => write_output(stream, FrameKind::Stderr, &b),
        Output::Exited(_) => Ok(()), // the pump reaps it properly
    }
}

/// Kills a live exec relay and delivers its `ExecExited`.
fn finish_exec(stream: &mut UnixStream, live_exec: &mut Option<(String, nsrun::ExecProc)>) {
    if let Some((exec_id, mut proc)) = live_exec.take() {
        proc.signal(9); // SIGKILL: 137 follows the 128+signal convention
        let _ = proc.wait_timeout(KILL_GRACE);
        let _ = send_event(stream, &Event::ExecExited { exec_id, code: 137 });
    }
}

/// Answers whatever is pending in the backlog with `engine busy` and closes
/// it. The one-container-at-a-time invariant is unchanged: the peer gets a
/// protocol error, never a session.
fn reply_busy(listener: &UnixListener) {
    for _ in 0..BUSY_BURST {
        let Ok((mut peer, _)) = listener.accept() else {
            return; // EAGAIN: the backlog is empty
        };
        let _ = send_error(&mut peer, "engine busy: one container at a time");
    }
}

/// SIGKILL the container, give it [`KILL_GRACE`] to be reaped, then remove
/// its cgroup. `Drop` stays the backstop if the reap itself fails.
fn stop(child: &mut Child) {
    child.kill();
    let _ = child.wait_timeout(KILL_GRACE);
    child.cleanup();
}

/// Best-effort last word: `write_polled` already bounds the wait, so a
/// peer that stopped reading cannot delay the engine's exit.
fn notify(stream: &mut UnixStream, message: &str) {
    let _ = send_event(
        stream,
        &Event::Error {
            message: message.into(),
        },
    );
}

fn send_event(stream: &mut UnixStream, event: &Event) -> io::Result<()> {
    let payload = encode_json(event).map_err(|e| io::Error::other(format!("encode: {e}")))?;
    wire_write(stream, FrameKind::Json, &payload)
}

fn send_error(stream: &mut UnixStream, message: &str) -> io::Result<()> {
    send_event(
        stream,
        &Event::Error {
            message: message.into(),
        },
    )
}

fn write_output(stream: &mut UnixStream, kind: FrameKind, bytes: &[u8]) -> io::Result<()> {
    wire_write(stream, kind, bytes)
}

/// write_frame over a non-blocking socket (polled writes).
fn wire_write(stream: &mut UnixStream, kind: FrameKind, payload: &[u8]) -> io::Result<()> {
    let mut v = (payload.len() as u32).to_le_bytes().to_vec();
    v.push(kind.as_u8());
    v.extend_from_slice(payload);
    write_polled(stream, &v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    static SEQ: AtomicU32 = AtomicU32::new(0);

    /// A private listener path per test; `std::env::temp_dir` is `/tmp`.
    fn listening() -> (UnixListener, UnixStream, PathBuf) {
        let path = std::env::temp_dir().join(format!(
            "engine-session-{}-{}.sock",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let listener = UnixListener::bind(&path).expect("bind");
        let client = UnixStream::connect(&path).expect("connect");
        (listener, client, path)
    }

    fn ctx() -> SessionCtx {
        SessionCtx {
            netns_default: None,
            log_default: None,
            mounts_default: Vec::new(),
        }
    }

    /// Test-side reader: the engine frames identically to engine-proto's
    /// canonical `read_frame` (u32 LE length + kind byte + payload).
    fn recv_event(stream: &mut UnixStream) -> Event {
        let frame = engine_proto::read_frame(stream)
            .expect("read frame")
            .expect("a frame, not EOF");
        decode_json(&frame.payload).expect("event json")
    }

    #[test]
    fn busy_reply_answers_a_pending_peer_then_closes_it() {
        let (listener, mut client, path) = listening();
        listener.set_nonblocking(true).expect("nonblocking");
        reply_busy(&listener);
        match recv_event(&mut client) {
            Event::Error { message } => {
                assert_eq!(message, "engine busy: one container at a time")
            }
            other => panic!("expected busy error, got {other:?}"),
        }
        // The peer is dropped, not parked: the next read is a clean EOF.
        assert!(engine_proto::read_frame(&mut client)
            .expect("eof read")
            .is_none());
        drop(client);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn an_empty_handshake_is_a_clean_continue() {
        let (listener, client, path) = listening();
        let (server_side, _) = listener.accept().expect("accept");
        // Half-close: the engine sees EOF instead of a wedged read timeout.
        client
            .shutdown(std::net::Shutdown::Write)
            .expect("shutdown");
        assert_eq!(
            handle(server_side, &listener, &ctx(), &mut None).expect("session"),
            Outcome::Continue
        );
        drop(client);
        let _ = std::fs::remove_file(path);
    }
}
