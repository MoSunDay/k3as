# storage-layer

The resource storage layer (T2.1/T2.2). NEW. Lives in `crates/storage/`.

## What it is

Defines the generic `StorageBackend` trait (`Watch` / `List` / `Create` /
`Update` / `Delete`) over the upstream `/registry/...` key layout, plus an
embedded in-process backend (`EmbeddedStorage`) implementing etcd-faithful
semantics with zero external dependencies.

## T2.1 spike decision

T2.1 asked to choose between (a) linking Go's etcd via FFI or (b)
supervising a bundled etcd subprocess (the k3s model). Both carry heavy
Go<->Rust integration cost. For a from-scratch Rust distribution we
instead implement the etcd SEMANTICS in pure Rust behind the trait. The
real etcd gRPC backend and the SQLite/KINE backend (T2.3) slot in as
alternative trait impls selected by `--datastore-endpoint` /
`--disable-etcd`, with the embedded store as the zero-dependency default
and the test double.

## Semantics (etcd-faithful)

- A single monotonic cluster-wide `Revision`; every successful write bumps
  it.
- Per key: `create_revision`, `mod_revision` (== Kubernetes
  `resourceVersion`), and `version` (write count since creation) -
  mirrors etcd's `KeyValue`.
- Optimistic concurrency: `update`/`delete` take an `if_revision` CAS
  (`kubectl apply` stale-revision conflicts).
- Watch via tokio broadcast.

## Module map (crates/storage/src/)

`backend.rs` (`StorageBackend`, `Watch`) | `embedded.rs`
(`EmbeddedStorage`) | `entry.rs` (`Revision`, `StoredEntry`, `WatchEvent`)
| `key.rs` (`Key`, `KeyPrefix`) | `error.rs` (`StorageError`). Re-exported
at the crate root.

## Status / next

- Landed: trait + `EmbeddedStorage` + 15 integration tests
  (`crates/storage/tests/embedded_storage.rs`). NOTE: the SSOT
  `plans/init-pro/index.md` status table still shows T2.1/T2.2
  "not-started" - stale, needs a lock-step update.
- Next: REST CRUD wiring (T1.2) so the discovery-only apiserver gains
  persistence; then the real etcd-backed impl.
- Current limitation: the embedded backend is live-watch only (no
  historical replay / compaction window).
