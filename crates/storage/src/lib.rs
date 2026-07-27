//! Resource storage backend (TODO **T2.1/T2.2**).
//!
//! Defines the [`StorageBackend`] trait -- a generic `Watch`/`List`/`Create`/
//! `Update`/`Delete` contract over the upstream `/registry/...` key layout --
//! and an embedded, in-process implementation ([`EmbeddedStorage`]) that
//! provides etcd-faithful revision + optimistic-concurrency semantics with
//! **zero external dependencies**.
//!
//! # Why an embedded backend first (T2.1 spike decision)
//!
//! T2.1 asked to choose between (a) linking Go's `etcd` via FFI or (b)
//! supervising a bundled `etcd` subprocess (the k3s model). Both carry heavy
//! Go<->Rust integration cost. For a *from-scratch Rust* distribution we
//! instead implement the etcd *semantics* (monotonic cluster revision,
//! create/mod revision per key, optimistic concurrency via CAS, watch via
//! broadcast) in pure Rust behind the same [`StorageBackend`] trait. The
//! etcd-backed impl (real gRPC client) and the SQLite/KINE impl (T2.3) slot in
//! as alternative trait impls selected by `--datastore-endpoint`/
//! `--disable-etcd`, with the embedded store as the zero-dependency default
//! and the test double.
//!
//! # ResourceVersion semantics
//!
//! Every successful write bumps a single monotonic cluster-wide [`Revision`].
//! Each key records `create_revision`, `mod_revision` (the revision of its
//! last write -> the Kubernetes `resourceVersion`), and `version` (count of
//! writes since creation), mirroring etcd's `KeyValue`. Optimistic concurrency
//! (`kubectl apply` stale-revision conflicts) is enforced by the `if_revision`
//! CAS parameter on `update`/`delete`.
#![forbid(unsafe_code)]

pub mod backend;
pub mod embedded;
pub mod entry;
pub mod error;
pub mod key;

pub use backend::{StorageBackend, Watch};
pub use embedded::EmbeddedStorage;
pub use entry::{Revision, StoredEntry, WatchEvent};
pub use error::StorageError;
pub use key::{Key, KeyPrefix};
