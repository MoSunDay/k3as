//! Built-in engine backend (T4.12, Q48-Q51): the kubelet `CriBackend`
//! implemented over per-container `init-pro engine serve` processes —
//! no containerd, no crictl.
//!
//! Process model (Q49): one engine process per sandbox (`--pause`,
//! holding a fresh netns) and one per container (joining
//! `/proc/<sandbox_pid>/ns/net`). The registry maps ids to sockets and
//! states; a pump task owns each running container's `RunStream` and
//! relays `Signal`/`Exec`/`Shutdown` through the SAME connection (Q50).
//! Exec output rides untagged kinds 2/3 delimited by
//! `ExecStarted`/`ExecExited`; container output is discarded here —
//! logs come from the CRI text file the engine appends to (Q50).
//! Images are real since T4.13 (Q53/Q54): registry-v2 pulls unpack
//! once into shared per-layer dirs and containers run on per-container
//! overlayfs views (fused-copy fallback when the kernel path is out).

pub mod backend;
pub mod exec;
pub mod image;
pub mod launch;
pub mod logs;
pub mod pump;
pub mod registry;
pub mod spec;

pub use backend::EngineBackend;
