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
- Watch: live events via tokio broadcast; `watch(prefix, Some(n))` REPLAYS
  retained history (bounded ring, default 10k revisions) from revision `n`
  (etcd-inclusive) before continuing live -- one lock-ordered seam, no
  gaps/dups (Sprint 12, T2.2).
- Compaction: `compact(rev)` advances the watermark; a watch start at/below
  it errors `Compacted` (upstream 410 Gone / Expired).
- `DELETED` watch events carry the object's final state (`prev`).
- Leader election does NOT use this layer's leases (none exist): see Q18
  (coordination.k8s.io Lease + resourceVersion CAS).

## Module map (crates/storage/src/)

`backend.rs` (`StorageBackend`, `Watch`) | `embedded.rs`
(`EmbeddedStorage`) | `entry.rs` (`Revision`, `StoredEntry`, `WatchEvent`)
| `history.rs` (event log + compaction watermark, private) | `key.rs`
(`Key`, `KeyPrefix`) | `error.rs` (`StorageError`). Re-exported at the
crate root.

## Status / next

- Landed: trait + `EmbeddedStorage` + 26 integration tests + 4 history unit
  tests (replay, compaction, seam losslessness; T2.1 + T2.2 both done in
  the SSOT).
- Wired into the apiserver since T1.2b (REST CRUD/watch/SSA); k8s watch
  `resourceVersion` semantics mapped at the REST layer ("events after N").
- Remaining limitation: in-memory / non-durable, and no real etcd-gRPC
  client - both are T2.3 (Q17).
