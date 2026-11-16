//! One client session (T4.11, Q48): handshake, then one container at a time.
//!
//! The engine owns a single container, so a second client must not be left
//! waiting in the listen backlog while the first one runs: the idle slice of
//! the container loop answers pending peers with `engine busy` and drops
//! them. Shutdown is observed in the same slice (see [`super::server`]).

use std::io;
use std::os::unix::net::{UnixListener, UnixStream};
use std::time::Duration;

use engine_proto::{
    decode_json, encode_json, read_frame, write_frame, ContainerSpec, Event, FrameKind, Request,
};
use nsrun::Output;

use crate::server::SHUTDOWN;

/// A peer that connects and sends 0-4 bytes must not wedge the engine: the
/// handshake frame is read under this timeout.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
/// A peer that stops reading must not block `write_frame` forever.
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);
/// Container event slice; also the shutdown and busy poll period.
const EVENT_SLICE: Duration = Duration::from_millis(200);
/// The busy notice is one small frame, so a peer that reads at all takes it
/// at once; the short cap only bounds a peer that stopped reading.
const BUSY_TIMEOUT: Duration = Duration::from_secs(1);
/// Pending peers answered per slice (a burst, then back to the container).
const BUSY_BURST: usize = 8;
/// Bound on the last-gasp notice so shutdown stays prompt.
const FINAL_TIMEOUT: Duration = Duration::from_millis(200);
/// SIGKILL budget before an unreaped container is left to `Drop`.
const KILL_GRACE: Duration = Duration::from_secs(5);

/// What the server should do after a session: keep accepting, or exit 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Continue,
    Exit,
}

/// Serves one connection. Returns [`Outcome::Exit`] when the peer asked for
/// shutdown or a shutdown signal arrived mid-container.
pub fn handle(mut stream: UnixStream, listener: &UnixListener) -> io::Result<Outcome> {
    // Without a read timeout a peer that opens the socket and sends nothing
    // blocks the single-threaded engine forever. Once a frame has been
    // parsed the timeout is cleared: a running container may legitimately go
    // quiet for minutes. The write timeout stays for the whole session.
    stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
    stream.set_write_timeout(Some(WRITE_TIMEOUT))?;
    let mut greeted = false;
    loop {
        let Some(frame) = read_frame(&mut stream)? else {
            return Ok(Outcome::Continue);
        };
        if !greeted {
            greeted = true;
            stream.set_read_timeout(None)?;
        }
        if frame.kind != FrameKind::Json {
            send_error(&mut stream, "unexpected binary frame on the request path")?;
            return Ok(Outcome::Continue);
        }
        let request: Request = match decode_json(&frame.payload) {
            Ok(r) => r,
            Err(e) => {
                send_error(&mut stream, &format!("bad request JSON: {e}"))?;
                return Ok(Outcome::Continue);
            }
        };
        match request {
            Request::Ping => send_event(&mut stream, &Event::Pong)?,
            Request::Shutdown => {
                send_event(&mut stream, &Event::Bye)?;
                return Ok(Outcome::Exit);
            }
            Request::Run { spec } => match run_one(&mut stream, spec, listener)? {
                Outcome::Continue => {}
                Outcome::Exit => return Ok(Outcome::Exit),
            },
        }
    }
}

fn run_one(
    stream: &mut UnixStream,
    spec: ContainerSpec,
    listener: &UnixListener,
) -> io::Result<Outcome> {
    let id = spec.id.clone();
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
    loop {
        if nsrun::shutdown_requested(&SHUTDOWN) {
            eprintln!("engine: shutdown signal; killing container {id}");
            notify(stream, "engine shutting down: container killed");
            stop(&mut child);
            return Ok(Outcome::Exit);
        }
        match child.next_event(EVENT_SLICE) {
            Ok(Some(Output::Stdout(bytes))) => {
                write_frame(stream, FrameKind::Stdout, &bytes)?;
            }
            Ok(Some(Output::Stderr(bytes))) => {
                write_frame(stream, FrameKind::Stderr, &bytes)?;
            }
            Ok(Some(Output::Exited(code))) => {
                child.cleanup();
                send_event(stream, &Event::Exited { id, code })?;
                return Ok(Outcome::Continue);
            }
            // An idle slice: the container said nothing, so this is the one
            // place a second client can be answered instead of hanging.
            Ok(None) => reply_busy(listener),
            Err(e) => {
                send_error(stream, &format!("container {id}: {e}"))?;
                child.kill();
                let _ = child.wait();
                child.cleanup();
                return Ok(Outcome::Continue);
            }
        }
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
        let _ = peer.set_write_timeout(Some(BUSY_TIMEOUT));
        let _ = send_error(&mut peer, "engine busy: one container at a time");
    }
}

/// SIGKILL the container, give it [`KILL_GRACE`] to be reaped, then remove
/// its cgroup. `Drop` stays the backstop if the reap itself fails.
fn stop(child: &mut nsrun::Child) {
    child.kill();
    let _ = child.wait_timeout(KILL_GRACE);
    child.cleanup();
}

/// Best-effort last word, bounded so a peer that stopped reading cannot
/// delay the engine's exit.
fn notify(stream: &mut UnixStream, message: &str) {
    let _ = stream.set_write_timeout(Some(FINAL_TIMEOUT));
    let _ = send_error(stream, message);
}

fn send_event(stream: &mut UnixStream, event: &Event) -> io::Result<()> {
    let payload = encode_json(event).map_err(|e| io::Error::other(format!("encode event: {e}")))?;
    write_frame(stream, FrameKind::Json, &payload)
}

fn send_error(stream: &mut UnixStream, message: &str) -> io::Result<()> {
    send_event(
        stream,
        &Event::Error {
            message: message.to_string(),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;
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

    fn recv_event(stream: &mut UnixStream) -> Event {
        let frame = read_frame(stream)
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
        assert!(read_frame(&mut client).expect("eof read").is_none());
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
            handle(server_side, &listener).expect("session"),
            Outcome::Continue
        );
        drop(client);
        let _ = std::fs::remove_file(path);
    }
}
