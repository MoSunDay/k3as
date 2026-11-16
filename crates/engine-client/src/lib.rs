//! Tokio client for the built-in engine's unix socket (T4.11, Q48).
//!
//! The daemon stays multi-threaded; only the engine process is strictly
//! single-threaded. This client speaks the framed protocol from
//! [`engine_proto`] and exposes a small async event pump.

#![forbid(unsafe_code)]

use std::io;
use std::path::Path;

use engine_proto::{
    decode_json, encode_json, ContainerSpec, Event, Frame, FrameKind, Request, MAX_FRAME,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

/// A connected engine. `run` consumes the connection: the P1 server streams
/// one container per session and then accepts the next request.
pub struct EngineClient {
    stream: UnixStream,
}

impl EngineClient {
    pub async fn connect(path: &Path) -> io::Result<Self> {
        Ok(Self {
            stream: UnixStream::connect(path).await?,
        })
    }

    /// Health check: `true` when the engine answered `pong`.
    pub async fn ping(mut self) -> io::Result<bool> {
        send_json(&mut self.stream, &Request::Ping).await?;
        match next_event(&mut self.stream).await? {
            Some(Event::Pong) => Ok(true),
            Some(Event::Error { message }) => Err(io::Error::other(message)),
            Some(other) => Err(io::Error::other(format!(
                "unexpected ping reply: {other:?}"
            ))),
            None => Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "engine closed before pong",
            )),
        }
    }

    pub async fn shutdown(mut self) -> io::Result<()> {
        send_json(&mut self.stream, &Request::Shutdown).await?;
        match next_event(&mut self.stream).await? {
            Some(Event::Bye) => Ok(()),
            Some(Event::Error { message }) => Err(io::Error::other(message)),
            Some(other) => Err(io::Error::other(format!(
                "unexpected shutdown reply: {other:?}"
            ))),
            None => Ok(()),
        }
    }

    /// Starts a container and returns its event stream.
    pub async fn run(mut self, spec: ContainerSpec) -> io::Result<RunStream> {
        let id = spec.id.clone();
        send_json(&mut self.stream, &Request::Run { spec }).await?;
        Ok(RunStream {
            stream: self.stream,
            id,
            done: false,
        })
    }
}

/// Streamed output + lifecycle events for one container.
pub enum StreamEvent {
    Started { pid: u32 },
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    Exited { code: i32 },
    Error(String),
}

pub struct RunStream {
    stream: UnixStream,
    id: String,
    done: bool,
}

impl RunStream {
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Next event. `Ok(None)` after the container exited (or on EOF).
    pub async fn next(&mut self) -> io::Result<Option<StreamEvent>> {
        if self.done {
            return Ok(None);
        }
        let Some(frame) = read_frame_async(&mut self.stream).await? else {
            self.done = true;
            return Ok(None);
        };
        match frame.kind {
            FrameKind::Stdout => Ok(Some(StreamEvent::Stdout(frame.payload))),
            FrameKind::Stderr => Ok(Some(StreamEvent::Stderr(frame.payload))),
            FrameKind::Json => {
                let event: Event = decode_json(&frame.payload)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                match event {
                    Event::Started { id, pid } => {
                        if id != self.id {
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                format!("engine started {id}, expected {}", self.id),
                            ));
                        }
                        Ok(Some(StreamEvent::Started { pid }))
                    }
                    Event::Exited { code, .. } => {
                        self.done = true;
                        Ok(Some(StreamEvent::Exited { code }))
                    }
                    Event::Error { message } => {
                        self.done = true;
                        Ok(Some(StreamEvent::Error(message)))
                    }
                    Event::Pong | Event::Bye => Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "unexpected control event on a run stream",
                    )),
                }
            }
            FrameKind::Stdin => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "engine sent a stdin frame",
            )),
        }
    }
}

async fn send_json(stream: &mut UnixStream, request: &Request) -> io::Result<()> {
    let payload = encode_json(request).map_err(|e| io::Error::other(format!("encode: {e}")))?;
    write_frame_async(stream, FrameKind::Json, &payload).await
}

async fn next_event(stream: &mut UnixStream) -> io::Result<Option<Event>> {
    let Some(frame) = read_frame_async(stream).await? else {
        return Ok(None);
    };
    if frame.kind != FrameKind::Json {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "expected a JSON control frame",
        ));
    }
    decode_json(&frame.payload)
        .map(Some)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

async fn write_frame_async(
    stream: &mut UnixStream,
    kind: FrameKind,
    payload: &[u8],
) -> io::Result<()> {
    if payload.len() > MAX_FRAME as usize {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "frame exceeds MAX_FRAME",
        ));
    }
    stream
        .write_all(&(payload.len() as u32).to_le_bytes())
        .await?;
    stream.write_all(&[kind.as_u8()]).await?;
    stream.write_all(payload).await?;
    stream.flush().await
}

async fn read_frame_async(stream: &mut UnixStream) -> io::Result<Option<Frame>> {
    let mut head = [0u8; 5];
    match stream.read_exact(&mut head[..1]).await {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    stream.read_exact(&mut head[1..]).await?;
    let len = u32::from_le_bytes([head[0], head[1], head[2], head[3]]);
    if len > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame length {len} exceeds MAX_FRAME"),
        ));
    }
    let kind = FrameKind::from_u8(head[4]).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unknown frame kind {}", head[4]),
        )
    })?;
    let mut payload = vec![0u8; len as usize];
    stream.read_exact(&mut payload).await?;
    Ok(Some(Frame { kind, payload }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};
    use tokio::net::UnixListener;

    fn socket_path(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!(
            "engine-client-{tag}-{}-{nanos}.sock",
            std::process::id()
        ))
    }

    #[tokio::test]
    async fn ping_round_trip() {
        let path = socket_path("ping");
        let listener = UnixListener::bind(&path).unwrap();
        let server = tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let frame = read_frame_async(&mut s).await.unwrap().unwrap();
            assert_eq!(frame.kind, FrameKind::Json);
            let _: Request = decode_json(&frame.payload).unwrap();
            let payload = encode_json(&Event::Pong).unwrap();
            write_frame_async(&mut s, FrameKind::Json, &payload)
                .await
                .unwrap();
        });
        let client = EngineClient::connect(&path).await.unwrap();
        assert!(client.ping().await.unwrap());
        server.await.unwrap();
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn run_stream_maps_raw_output_and_exit() {
        let path = socket_path("run");
        let listener = UnixListener::bind(&path).unwrap();
        let server = tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let frame = read_frame_async(&mut s).await.unwrap().unwrap();
            let Request::Run { spec } = decode_json(&frame.payload).unwrap() else {
                panic!("expected run");
            };
            let started = encode_json(&Event::Started {
                id: spec.id,
                pid: 42,
            })
            .unwrap();
            write_frame_async(&mut s, FrameKind::Json, &started)
                .await
                .unwrap();
            write_frame_async(&mut s, FrameKind::Stdout, b"hello")
                .await
                .unwrap();
            let exited = encode_json(&Event::Exited {
                id: "c1".into(),
                code: 7,
            })
            .unwrap();
            write_frame_async(&mut s, FrameKind::Json, &exited)
                .await
                .unwrap();
        });
        let client = EngineClient::connect(&path).await.unwrap();
        let mut run = client
            .run(ContainerSpec::new("c1", "/r", vec!["/bin/true".into()]))
            .await
            .unwrap();
        assert!(matches!(
            run.next().await.unwrap(),
            Some(StreamEvent::Started { pid: 42 })
        ));
        match run.next().await.unwrap() {
            Some(StreamEvent::Stdout(b)) => assert_eq!(b, b"hello"),
            _ => panic!("expected stdout"),
        }
        assert!(matches!(
            run.next().await.unwrap(),
            Some(StreamEvent::Exited { code: 7 })
        ));
        assert!(run.next().await.unwrap().is_none());
        server.await.unwrap();
        let _ = std::fs::remove_file(path);
    }
}
