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
//! - [`server`] — the local log/exec listener (T4.2 Scope C, Q31) the
//!   apiserver relay dials on `node_ip:kubelet_port`.
//! - [`log_rotate`] — T4.6/Q39 pod-log date layout helpers + the
//!   copytruncate rotation sweep (100 MiB / 5 generations).
//! - [`log_retention`] -- C4 age-based retention of per-date pod-log dirs
//!   (`STALE_LOG_DAYS`, default 7); the current date is never touched.
//! - [`drain`] -- C2 bounded shutdown drain: every loop gets
//!   `DRAIN_JOIN_TIMEOUT` (10s) and is aborted + named if it overstays.
//! - [`volumes`] — pod volume materialization (emptyDir dirs +
//!   fingerprint-refreshed configMap/secret key files, T4.2 Scope B) and
//!   the C4 pod teardown: the WHOLE `<log-root>/<uid>` dir (date logs
//!   included) is released once the sandbox is gone, uid-validated.
//! - [`env`] + [`env_fetch`] — container env/envFrom projection
//!   (T4.7): pure parse/resolve + the create-time fetch/event side.
//! - [`resources`] — container resources → CRI cgroup knobs + pod QoS
//!   classification (T4.9; upstream milliCPUToShares/milliCPUToQuota +
//!   GetPodQOS, quantities parsed via `common`).
//! - [`image_gc`] -- C3 disk-pressure-aware image GC seam: the sync loop
//!   hands the backend the desired pod images once per cycle and the
//!   builtin engine frees the CAS store when the image root fills.
//! - [`host_mounts`] — the `init-pro.io/host-mounts` pod annotation
//!   (T4.14/Q55): host-dir bind mounts parsed and validated purely;
//!   bind failure is the permission verdict (node creds decide).
//! - [`gpu`] — whole-GPU inventory/ledger/publication + env injection
//!   (T7.2, Q61; fake-scope, real-hardware closure stays OPEN).
//! - [`runner`] — the four loops (watch, sync, node/lease, local API) +
//!   [`spawn`].
#![forbid(unsafe_code)]

pub mod agent_broker;
pub mod agent_exec;
pub mod agent_exec_sandbox;
#[cfg(test)] // T7.7 (Q63) hardened-sandbox pure-selection cases
mod agent_exec_sandbox_tests;
pub mod agent_objects;
pub mod agent_sandbox;
pub mod agent_sandbox_plan;
#[cfg(test)] // T7.7 (Q63) hardened-sandbox spec/tamper cases
mod agent_sandbox_tests;
pub mod agents;
pub mod config;
pub mod cri_backend;
pub mod drain;
pub mod engine;
pub mod env;
pub mod env_fetch;
pub mod exec;
pub mod framing;
pub mod gpu;
pub mod host_mounts;
pub mod http;
pub mod image_gc;
pub mod log_retention;
pub mod log_rotate;
pub mod nfs_mount;
pub mod node;
pub(crate) mod node_status;
pub mod objects;
pub mod probes;
pub mod resources;
pub mod runner;
pub mod server;
#[cfg(test)] // pure-helper cases split out of server.rs (file size limit)
mod server_tests;
pub mod status;
pub mod sync;
pub mod volumes;
pub mod watch;
mod watch_backoff;

pub use agent_sandbox::{select_sandbox, SandboxNodeConfig, SandboxSelection};
pub use agents::spawn_agent_runner;
pub use agents::AgentRunnerConfig;
pub use config::{detect_outbound_ip, KubeletConfig};
pub use cri_backend::{ContainerView, CriBackend, CriCtlBackend, SandboxView};
pub use drain::{drain, join_bounded, DrainOutcome, DRAIN_JOIN_TIMEOUT, TASK_NAMES};
pub use env::{
    config_error_event, parse_env, parse_env_from, resolve_env, EnvFromSource, EnvSources,
    EnvValue, EnvVar, RefKind,
};
pub use env_fetch::{resolve_table, EnvOutcome, EnvTable};
pub use exec::{execute, snapshot};
pub use gpu::{parse_inventory, GpuInventorySource, GpuRuntime};
pub use host_mounts::{collect, HostMount, HOST_MOUNTS_ANNOTATION};
pub use http::{HttpError, HttpJson};
pub use objects::{
    node_object, pod_view, ContainerSpec, PodView, VolumeKind, VolumeMount, VolumeSpec,
};
pub use probes::{HttpGetSpec, PodProbeState, ProbeHandler, ProbeSpec};
pub use resources::{linux_resources, parse_requirements, qos_class, QoS, ResourceRequirements};
pub use runner::{default_node_name, spawn};
pub use server::serve;
pub use status::{build_pod_status, merge_pod_for_status, status_semantically_eq};
pub use sync::{plan, Action, Snapshot};
pub use volumes::{
    cleanup_volumes, empty_dir_path, ensure_volumes, nfs_volume_path, pod_dir, resolve_mounts,
    volume_root,
};
pub use watch::WatchConn;
