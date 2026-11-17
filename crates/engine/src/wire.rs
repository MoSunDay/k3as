//! Mid-run peer multiplexing (T4.12, Q49/Q50): while a container runs,
//! the engine polls the SAME client socket for `Signal`/`Exec`/`Ping`/
//! `Shutdown` instead of blocking in `read_frame`.
//!
//! The socket stays O_NONBLOCK for the whole run; blocked writes wait on
//! writability through `nsrun::poll_writable` (gate 3), so a peer that
//! stopped reading still only costs one bounded slice. Frames may arrive
//! byte-dribbled, so raw bytes accumulate in [`FrameBuffer`] until one
//! whole frame parses.

use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::time::Duration;

use engine_proto::{Frame, FrameKind, MAX_FRAME};

/// How long a blocked write may stall the engine before it counts as a
/// dead peer.
pub(crate) const WRITE_SLICE: Duration = Duration::from_secs(5);
/// Read scratch size; a poll that finds nothing pays one syscall.
const READ_CHUNK: usize = 8 * 1024;

/// What a poll of the client socket found.
#[derive(Debug)]
pub(crate) enum PeerInput {
    /// Nothing readable right now (EAGAIN): an idle slice.
    Idle,
    /// The peer closed the connection mid-run.
    Eof,
    /// One whole frame arrived.
    Frame(Frame),
}

/// Raw byte accumulator + whole-frame parser.
pub(crate) struct FrameBuffer {
    buf: Vec<u8>,
}

impl FrameBuffer {
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }

    /// Drains the socket once (non-blocking) and returns the next whole
    /// frame if one completed. A frame followed by close is delivered
    /// before `Eof`; a truncated frame reports `Eof` (dead peer).
    pub fn poll(&mut self, stream: &mut UnixStream) -> io::Result<PeerInput> {
        let mut scratch = [0u8; READ_CHUNK];
        loop {
            match stream.read(&mut scratch) {
                Ok(0) => {
                    return match try_parse(&mut self.buf)? {
                        Some(frame) => Ok(PeerInput::Frame(frame)),
                        None => Ok(PeerInput::Eof),
                    };
                }
                Ok(n) => {
                    self.buf.extend_from_slice(&scratch[..n]);
                    if let Some(frame) = try_parse(&mut self.buf)? {
                        return Ok(PeerInput::Frame(frame));
                    }
                    // Loop: a large frame may need several reads.
                    if n < READ_CHUNK {
                        return Ok(PeerInput::Idle);
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(PeerInput::Idle),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
    }
}

/// Parses one whole frame off the front of `buf`; `None` when the buffer
/// still holds less than a frame.
fn try_parse(buf: &mut Vec<u8>) -> io::Result<Option<Frame>> {
    if buf.len() < 5 {
        return Ok(None);
    }
    let len = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    if len > MAX_FRAME as usize {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "peer frame exceeds MAX_FRAME",
        ));
    }
    if buf.len() < 5 + len {
        return Ok(None);
    }
    let kind = FrameKind::from_u8(buf[4]).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unknown frame kind {}", buf[4]),
        )
    })?;
    let payload = buf[5..5 + len].to_vec();
    buf.drain(..5 + len);
    Ok(Some(Frame { kind, payload }))
}

/// `write_all` on a non-blocking socket: waits for writability in
/// [`WRITE_SLICE`] slices instead of returning `WouldBlock`.
pub(crate) fn write_polled(stream: &mut UnixStream, bytes: &[u8]) -> io::Result<()> {
    let mut written = 0;
    while written < bytes.len() {
        match stream.write(&bytes[written..]) {
            Ok(n) => written += n,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                if !nsrun::poll_writable(stream.as_raw_fd(), WRITE_SLICE)? {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "peer stopped reading",
                    ));
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(kind: FrameKind, payload: &[u8]) -> Vec<u8> {
        let mut v = (payload.len() as u32).to_le_bytes().to_vec();
        v.push(kind.as_u8());
        v.extend_from_slice(payload);
        v
    }

    #[test]
    fn parser_assembles_dribbled_frames_and_keeps_remainders() {
        let mut buf = FrameBuffer::new();
        buf.buf.extend_from_slice(&frame(FrameKind::Json, b"{}"));
        buf.buf
            .extend_from_slice(&frame(FrameKind::Stdout, b"ab")[..3]);
        let f = try_parse(&mut buf.buf).unwrap().expect("first frame");
        assert_eq!(f.kind, FrameKind::Json);
        assert_eq!(f.payload, b"{}");
        assert!(try_parse(&mut buf.buf).unwrap().is_none());
        buf.buf
            .extend_from_slice(&frame(FrameKind::Stdout, b"ab")[3..]);
        let f = try_parse(&mut buf.buf).unwrap().expect("second frame");
        assert_eq!(f.kind, FrameKind::Stdout);
        assert_eq!(f.payload, b"ab");
        assert!(buf.buf.is_empty());
    }

    #[test]
    fn parser_rejects_oversized_and_unknown_kinds() {
        let mut b = (engine_proto::MAX_FRAME + 1).to_le_bytes().to_vec();
        b.push(FrameKind::Json.as_u8());
        assert!(try_parse(&mut b).is_err());
        let mut b = frame(FrameKind::Stdout, b"x");
        b[4] = 9;
        assert!(try_parse(&mut b).is_err());
    }

    #[test]
    fn eof_after_a_frame_delivers_the_frame_first() {
        let (mut a, mut b) = std::os::unix::net::UnixStream::pair().unwrap();
        use std::io::Write as _;
        a.write_all(&frame(FrameKind::Json, b"hi")).unwrap();
        drop(a); // peer closes right after the frame
        let mut fb = FrameBuffer::new();
        match fb.poll(&mut b).unwrap() {
            PeerInput::Frame(f) => assert_eq!(f.payload, b"hi"),
            other => panic!("expected frame, got {other:?}"),
        }
        assert!(matches!(fb.poll(&mut b).unwrap(), PeerInput::Eof));
    }

    #[test]
    fn idle_slice_is_not_an_error() {
        let (a, mut b) = std::os::unix::net::UnixStream::pair().unwrap();
        b.set_nonblocking(true).unwrap();
        let mut fb = FrameBuffer::new();
        assert!(matches!(fb.poll(&mut b).unwrap(), PeerInput::Idle));
        drop(a);
    }
}
