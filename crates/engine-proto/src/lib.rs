//! Internal control protocol between the init-pro daemon and the built-in
//! engine process (TODO **T4.11**, decision **Q48**).
//!
//! One frame is `u32 LE length | u8 kind | payload`. `kind = 1` is JSON
//! ([`Request`]/[`Event`]); kinds 2/3/4 are raw stdout/stderr/stdin bytes,
//! so non-UTF-8 container output never pays base64. This is an internal
//! byte channel, not an apiserver wire format (Q10 governs those only).

#![forbid(unsafe_code)]

use std::io::{self, Read, Write};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// Kind byte of a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FrameKind {
    Json = 1,
    Stdout = 2,
    Stderr = 3,
    Stdin = 4,
}

impl FrameKind {
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    pub const fn from_u8(v: u8) -> Option<Self> {
        match v {
            1 => Some(Self::Json),
            2 => Some(Self::Stdout),
            3 => Some(Self::Stderr),
            4 => Some(Self::Stdin),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub kind: FrameKind,
    pub payload: Vec<u8>,
}

/// Defensive cap: a corrupt length must not allocate gigabytes.
pub const MAX_FRAME: u32 = 16 * 1024 * 1024;

pub fn write_frame<W: Write>(w: &mut W, kind: FrameKind, payload: &[u8]) -> io::Result<()> {
    if payload.len() > MAX_FRAME as usize {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "frame exceeds MAX_FRAME",
        ));
    }
    w.write_all(&(payload.len() as u32).to_le_bytes())?;
    w.write_all(&[kind.as_u8()])?;
    w.write_all(payload)?;
    w.flush()
}

/// Reads one frame. `Ok(None)` is a clean EOF before any byte.
pub fn read_frame<R: Read>(r: &mut R) -> io::Result<Option<Frame>> {
    let mut head = [0u8; 5];
    match r.read_exact(&mut head[..1]) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    r.read_exact(&mut head[1..])?;
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
    r.read_exact(&mut payload)?;
    Ok(Some(Frame { kind, payload }))
}

pub fn encode_json<T: Serialize>(value: &T) -> Result<Vec<u8>, serde_json::Error> {
    serde_json::to_vec(value)
}

pub fn decode_json<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, serde_json::Error> {
    serde_json::from_slice(bytes)
}

/// One bind mount inside the container.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MountSpec {
    pub source: String,
    /// Absolute container path.
    pub target: String,
    #[serde(default)]
    pub readonly: bool,
}

/// What to run. Mirrors the CRI fields the kubelet already carries, minus
/// the P2+ fields (labels, log path, CNI result).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContainerSpec {
    pub id: String,
    pub rootfs: String,
    pub argv: Vec<String>,
    #[serde(default)]
    pub env: Vec<String>,
    #[serde(default)]
    pub hostname: Option<String>,
    #[serde(default = "default_cwd")]
    pub cwd: String,
    #[serde(default)]
    pub uid: u32,
    #[serde(default)]
    pub gid: u32,
    #[serde(default)]
    pub mounts: Vec<MountSpec>,
    /// Fresh net namespace; P2 joins the CNI sandbox netns instead.
    #[serde(default)]
    pub new_net: bool,
    #[serde(default = "default_true")]
    pub no_new_privs: bool,
    #[serde(default)]
    pub cgroup_name: Option<String>,
    #[serde(default)]
    pub memory_max: Option<u64>,
    #[serde(default)]
    pub cpu_max: Option<String>,
    #[serde(default)]
    pub pids_max: Option<u64>,
}

impl ContainerSpec {
    pub fn new(id: impl Into<String>, rootfs: impl Into<String>, argv: Vec<String>) -> Self {
        Self {
            id: id.into(),
            rootfs: rootfs.into(),
            argv,
            env: Vec::new(),
            hostname: None,
            cwd: default_cwd(),
            uid: 0,
            gid: 0,
            mounts: Vec::new(),
            new_net: false,
            no_new_privs: true,
            cgroup_name: None,
            memory_max: None,
            cpu_max: None,
            pids_max: None,
        }
    }
}

fn default_cwd() -> String {
    "/".to_string()
}

const fn default_true() -> bool {
    true
}

// One request per session: the extra stack bytes are cheaper than a Box.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Ping,
    Run { spec: ContainerSpec },
    Shutdown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    Pong,
    Started { id: String, pid: u32 },
    Exited { id: String, code: i32 },
    Error { message: String },
    Bye,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn frame_round_trip_raw_and_json() {
        let mut bytes = Vec::new();
        write_frame(&mut bytes, FrameKind::Stdout, b"hello\0world").unwrap();
        let mut cur = Cursor::new(bytes);
        let f = read_frame(&mut cur).unwrap().unwrap();
        assert_eq!(f.kind, FrameKind::Stdout);
        assert_eq!(f.payload, b"hello\0world");
        assert!(read_frame(&mut cur).unwrap().is_none());
    }

    #[test]
    fn request_round_trip() {
        let req = Request::Run {
            spec: ContainerSpec::new("c1", "/srv/rootfs", vec!["/bin/echo".into()]),
        };
        let raw = encode_json(&req).unwrap();
        assert_eq!(decode_json::<Request>(&raw).unwrap(), req);
    }

    #[test]
    fn defaults_survive_old_payloads() {
        let raw = br#"{"op":"run","spec":{"id":"c1","rootfs":"/r","argv":["/bin/true"]}}"#;
        let Request::Run { spec } = decode_json::<Request>(raw).unwrap() else {
            panic!("expected run");
        };
        assert_eq!(spec.cwd, "/");
        assert!(spec.no_new_privs);
        assert!(spec.mounts.is_empty());
    }

    #[test]
    fn oversized_frame_is_rejected() {
        let mut bytes = (MAX_FRAME + 1).to_le_bytes().to_vec();
        bytes.push(FrameKind::Json.as_u8());
        assert!(read_frame(&mut Cursor::new(bytes)).is_err());
    }

    #[test]
    fn truncated_frame_is_an_error() {
        let mut bytes = 8u32.to_le_bytes().to_vec();
        bytes.push(FrameKind::Json.as_u8());
        bytes.extend_from_slice(b"abc");
        assert!(read_frame(&mut Cursor::new(bytes)).is_err());
    }
}
