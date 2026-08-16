//! kubelet equivalent (TODO **T4.2**, decisions **Q26**/**Q27**).
//!
//! Watches Pods assigned to this node over the apiserver HTTP surface,
//! reconciles the desired pod set through a CRI backend trait (vendored-crictl
//! subprocess today — Q26 route B, native gRPC later — Q27 keeps the seam),
//! writes Pod status back through the `/status` subresource, and registers
//! the Node plus a `kube-node-lease` heartbeat Lease.
//!
//! Layering (pure-functional; errors as values; no OOP):
//! - [`http`] — minimal HTTP/1.1 client (Q21 parity): PUT/POST + chunked
//!   watch streaming; [`framing`] holds the pure response/chunked parsers.
//! - [`objects`] — pure Pod/Node/Lease JSON builders and readers.
//! - [`cri_backend`] — the [`CriBackend`] trait + the [`CriCtlBackend`]
//!   adapter over [`runtime::CriCtl`].
//! - [`sync`] + [`exec`] — the reconcile core: pure `plan()` (desired vs
//!   snapshot -> actions) and `execute()` (sequential CRI application).
//! - [`status`] — Pod status construction + semantic equality.
//! - [`runner`] — the three loops (watch, sync, node/lease) + [`spawn`].
#![forbid(unsafe_code)]

pub mod cri_backend;
pub mod exec;
pub mod framing;
pub mod http;
pub mod node;
pub mod objects;
pub mod runner;
pub mod status;
pub mod sync;
pub mod watch;

pub use cri_backend::{ContainerView, CriBackend, CriCtlBackend, SandboxView};
pub use exec::{execute, snapshot};
pub use http::{HttpError, HttpJson};
pub use objects::{node_object, pod_view, ContainerSpec, PodView};
pub use runner::{default_node_name, spawn, KubeletConfig};
pub use status::{build_pod_status, merge_pod_for_status, status_semantically_eq};
pub use sync::{plan, Action, Snapshot};
pub use watch::WatchConn;
