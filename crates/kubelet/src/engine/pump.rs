//! The per-container pump task (T4.12, Q49/Q50): owns the `RunStream`
//! (the container's one connection), forwards control requests onto it,
//! and translates lifecycle events into registry states.
//!
//! Container stdout/stderr frames are NOT forwarded anywhere: the CRI
//! log file (written by the engine, Q50) is the source for `kubectl
//! logs`. Frames that arrive inside an exec window belong to the exec
//! reply; outside one they are drained.

use std::io;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::time::Duration;

use engine_client::{EngineClient, RunStream, StreamEvent};
use tokio::sync::{mpsc, oneshot};

use super::launch;
use super::registry::Registry;

/// Control requests funneled into the pump (the engine takes ONE
/// connection at a time; everything rides it — Q50).
pub enum Ctrl {
    /// Deliver a signal to the container.
    Signal(i32),
    /// Exec one command; the reply carries merged output + exit code.
    Exec {
        cmd: Vec<String>,
        reply: oneshot::Sender<Result<(i32, String), String>>,
    },
    /// Kill the container: the engine answers Exited 143 + Bye and exits.
    Shutdown,
}

/// Spawn the pump for a started container. `stream` is the post-`Run`
/// connection; `child` is the engine process, reaped here on teardown.
pub(crate) fn spawn_pump(
    registry: Registry,
    id: String,
    socket: PathBuf,
    mut stream: RunStream,
    child: Child,
) -> mpsc::Sender<Ctrl> {
    let (tx, mut rx) = mpsc::channel::<Ctrl>(8);
    registry.set_engine_pid(&id, child.id());
    tokio::spawn(async move {
        let mut exec: Option<ExecWindow> = None;
        loop {
            tokio::select! {
                ctrl = rx.recv() => {
                    match ctrl {
                        Some(Ctrl::Signal(signo)) => {
                            if stream.send_signal(&id, signo).await.is_err() {
                                break;
                            }
                        }
                        Some(Ctrl::Shutdown) => {
                            let _ = stream.send_shutdown().await;
                            // Exited(143) + Bye arrive on the stream path.
                        }
                        Some(Ctrl::Exec { cmd, reply }) => {
                            if exec.is_some() {
                                let _ = reply.send(Err(
                                    "exec busy: one in-flight exec per engine (Q50)"
                                        .to_string(),
                                ));
                            } else {
                                exec = Some(ExecWindow { reply: Some(reply), out: Vec::new() });
                                if let Err(e) =
                                    stream.send_exec(&id, cmd, Vec::new(), "/".into()).await
                                {
                                    ExecWindow::answer(exec.take(), Err(e.to_string()));
                                }
                            }
                        }
                        None => { // Backend dropped the channel: teardown.
                            let _ = stream.send_shutdown().await;
                        }
                    }
                }
                ev = stream.next() => {
                    match ev {
                        Ok(Some(StreamEvent::Started { pid })) => {
                            registry.set_running(&id, pid);
                        }
                        Ok(Some(StreamEvent::Stdout(b)))
                        | Ok(Some(StreamEvent::Stderr(b))) => {
                            if let Some(w) = exec.as_mut() {
                                w.push(&b);
                            }
                        }
                        Ok(Some(StreamEvent::ExecStarted { .. })) => {}
                        Ok(Some(StreamEvent::ExecExited { code, .. })) => {
                            let window = exec.take();
                            let out = window
                                .as_ref()
                                .map(|w| String::from_utf8_lossy(&w.out).into_owned())
                                .unwrap_or_default();
                            ExecWindow::answer(window, Ok((code, out)));
                        }
                        Ok(Some(StreamEvent::Exited { code })) => {
                            // Natural or signalled exit: answer any exec
                            // with the container's code, then make the
                            // engine exit (Shutdown on the same session).
                            let window = exec.take();
                            let out = window
                                .as_ref()
                                .map(|w| String::from_utf8_lossy(&w.out).into_owned())
                                .unwrap_or_default();
                            ExecWindow::answer(window, Ok((code, out)));
                            registry.set_exited(&id, code);
                            let _ = stream.send_shutdown().await;
                            break;
                        }
                        Ok(Some(StreamEvent::Error(message))) => {
                            ExecWindow::answer(exec.take(), Err(message.clone()));
                            tracing::warn!(
                                target: "init-pro", container = %id, %message,
                                "builtin engine error"
                            );
                        }
                        Ok(None) | Err(_) => break,
                    }
                }
            }
        }
        // Teardown: the engine exits on Bye after Shutdown (EOF here
        // means it already did or died); reap (off the workers -- D, the
        // bounded wait must not park this task's worker), clear socket +
        // registry.
        let _ = launch::reap_offload(child, Duration::from_secs(5)).await;
        let _ = std::fs::remove_file(&socket);
        registry.set_exited_if_running(&id);
        registry.clear_ctrl(&id);
    });
    tx
}

/// A pending exec: frames captured between ExecStarted/ExecExited.
struct ExecWindow {
    reply: Option<oneshot::Sender<Result<(i32, String), String>>>,
    out: Vec<u8>,
}

impl ExecWindow {
    fn push(&mut self, bytes: &[u8]) {
        // Cap merged exec output (no unbounded buffering on a chatty
        // container); like the CRI relay, the tail is what matters.
        const MAX: usize = 1024 * 1024;
        if self.out.len() + bytes.len() <= MAX {
            self.out.extend_from_slice(bytes);
        }
    }

    fn answer(window: Option<Self>, result: Result<(i32, String), String>) {
        if let Some(w) = window {
            if let Some(reply) = w.reply {
                let _ = reply.send(result);
            }
        }
    }
}

/// Probe one engine socket (Q51 shape, tighter budget): `Ok(Some(pid))`
/// on a pause engine, `Ok(None)` on a plain one.
pub(crate) async fn probe(socket: &Path) -> io::Result<Option<u32>> {
    let client = EngineClient::connect(socket).await?;
    tokio::time::timeout(Duration::from_secs(2), client.ping_pid())
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "engine ping timed out"))?
}
