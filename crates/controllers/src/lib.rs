//! Controller-manager framework (TODO **T3.1a**).
//!
//! The first slice of kube-controller-manager-equivalent control loops:
//!
//! * [`client`] -- a small [`Client`] abstraction over the storage trait
//!   (decision **Q19**: v1 controllers share the apiserver's storage in
//!   process; an HTTP-backed client arrives with T3.4);
//! * [`informer`] + [`workqueue`] -- the LIST -> WATCH reflector and
//!   client-go workqueue semantics that drive every reconciler;
//! * [`leaderelection`] -- coordination.k8s.io Lease + resourceVersion CAS
//!   (decision **Q18**);
//! * [`controllers`] -- ReplicaSet / Deployment / Endpoints / StatefulSet
//!   / DaemonSet reconcilers (T3.1a + T3.1b, done Sprint 14; incl. GC +
//!   namespace lifecycle, decision Q20);
//! * [`runner`] -- [`ControllerManager`] wiring the whole set together
//!   (leadership supervisor); its informer fan-out table lives in
//!   [`runner_wiring`] and the worker loop + per-kind dispatch in
//!   [`runner_worker`] (A5 split, T7.3/Q35).
//!
//! Wire format is JSON-only (decision **Q10**): every object flowing through
//! the framework is a raw `serde_json::Value`, canonical via BTreeMap maps.
#![forbid(unsafe_code)]

pub mod client;
pub mod controllers;
pub mod error;
pub mod id;
pub mod informer;
pub mod leaderelection;
pub mod object;
pub mod runner;
/// A5 split (T7.3, Q35): the per-resource informer fan-out table +
/// workqueue/worker spawning, moved verbatim out of `runner`.
pub(crate) mod runner_wiring;
/// A5 split (T7.3, Q35): reconciler workers, the GC delete hook, the
/// child -> owner enqueuer and `reconcile_key` dispatch.
pub(crate) mod runner_worker;
pub mod stop;
pub mod time;
pub mod workqueue;

pub use client::{Client, StorageClient};
pub use controllers::node_cidr;
pub use controllers::Caches;
pub use error::ControllerError;
pub use informer::{EventHandler, Informer, ObjectStore};
pub use leaderelection::{LeaderElector, LeaseConfig};
pub use runner::{ControllerManager, ManagerConfig};
pub use stop::Stop;
pub use workqueue::WorkQueue;
