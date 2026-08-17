//! Resource storage backends (T2.1/T2.2/T2.3 done).
//!
//! Defines the [`StorageBackend`] trait -- a generic `Watch`/`List`/`Create`/
//! `Update`/`Delete` contract over the upstream `/registry/...` key layout --
//! plus two interchangeable implementations with identical etcd-faithful
//! semantics (monotonic cluster revision, create/mod revision per key,
//! optimistic concurrency via CAS, watch replay + compaction):
//!
//! - [`EmbeddedStorage`] -- in-process, zero external dependencies; the
//!   default backend and the test double.
//! - [`SqliteStorage`] -- a libsql/SQLite (kine-style) backend selected by
//!   `--datastore-endpoint sqlite://...` (T2.3, Q29): one append-only `kv`
//!   table whose row id IS the cluster revision, persisted to a local
//!   database file for zero-restart durability. Local-file mode only -- no
//!   remote/replication/sync features are compiled in.
//!
//! # Why an embedded backend first (T2.1 spike decision)
//!
//! T2.1 asked to choose between (a) linking Go's `etcd` via FFI or (b)
//! supervising a bundled `etcd` subprocess (the k3s model). Both carry heavy
//! Go<->Rust integration cost. For a *from-scratch Rust* distribution we
//! instead implement the etcd *semantics* (monotonic cluster revision,
//! create/mod revision per key, optimistic concurrency via CAS, watch via
//! broadcast) in pure Rust behind the same [`StorageBackend`] trait. The
//! etcd-gRPC client impl moved to T3.4 (superseding the original
//! `--disable-etcd` plan, Q17); the SQLite/KINE impl landed as T2.3.
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
mod history;
pub mod key;
pub mod sqlite;

pub use backend::{StorageBackend, Watch};
pub use embedded::EmbeddedStorage;
pub use entry::{Revision, StoredEntry, WatchEvent};
pub use error::StorageError;
pub use key::{Key, KeyPrefix};
pub use sqlite::SqliteStorage;
