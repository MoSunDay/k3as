# storage-layer

The resource storage layer (T2.1/T2.2/T2.3, all done). Lives in
`crates/storage/`.

## What it is

Defines the generic `StorageBackend` trait (`Watch` / `List` / `Create` /
`Update` / `Delete`) over the upstream `/registry/...` key layout, plus TWO
interchangeable backends with identical etcd-faithful semantics: the
embedded in-process store (`EmbeddedStorage`, zero external dependencies,
the default + test double) and a durable RocksDB backend (`RdbStorage`,
T2.3/Q33) selected by `--datastore-endpoint rdb://<path>`. (The
Sprint-19 libsql/SQLite backend was removed in Sprint 25; `sqlite://`
answers `Unsupported`.)

## T2.1 spike decision

T2.1 asked to choose between (a) linking Go's etcd via FFI or (b)
supervising a bundled etcd subprocess (the k3s model). Both carry heavy
Go<->Rust integration cost. For a from-scratch Rust distribution we
instead implement the etcd SEMANTICS in pure Rust behind the trait. The
durable backend slot is rdb/RocksDB as of T2.3/Q33 (Sprint 25 — the
kine-style append-only model now rides the rdb `store` engine); the real
etcd gRPC client is re-scoped to T3.4 (HA multi-server, its only
consumer — Q17 supersede note). Backend selection is
`--datastore-endpoint` only; empty/None keeps the embedded default.

## Semantics (etcd-faithful, both backends)

- A single monotonic cluster-wide `Revision`; every successful write bumps
  it.
- Per key: `create_revision`, `mod_revision` (== Kubernetes
  `resourceVersion`), and `version` (write count since creation) -
  mirrors etcd's `KeyValue`. RdbStorage persists the entry (with its
  explicit `version`) as the live `obj/<key>` value, so compaction
  cannot alter it.
- Optimistic concurrency: `update`/`delete` take an `if_revision` CAS
  (`kubectl apply` stale-revision conflicts).
- Watch: live events via tokio broadcast; `watch(prefix, Some(n))` REPLAYS
  retained history from revision `n` (etcd-inclusive) before continuing
  live -- one lock-ordered seam, no gaps/dups. Embedded: bounded ring,
  default 10k revisions (Sprint 12, T2.2). RocksDB: replay scans the
  append-only `log/` prefix (`log/<rev big-endian>` — lexicographic
  order IS revision order), so it survives restart; broadcast is
  strictly post-commit per fsync-ed batch (Sprint 25, T2.3).
- Compaction: `compact(rev)` advances the watermark; a watch start at/below
  it errors `Compacted` (upstream 410 Gone / Expired). RocksDB keeps
  `obj/<key>` as the live value and persists the watermark to
  `meta/compact`.
- `DELETED` watch events carry the object's final state (`prev`).
- Leader election does NOT use this layer's leases (none exist): see Q18
  (coordination.k8s.io Lease + resourceVersion CAS).

## Module map (crates/storage/src/)

`backend.rs` (`StorageBackend`, `Watch`) | `embedded.rs`
(`EmbeddedStorage`) | `rdb_backend/` (`mod.rs` `RdbStorage` on the rdb
crate `store` engine + `keys.rs` `meta/`·`obj/`·`log/` key layout +
`watch.rs` replay) | `entry.rs` (`Revision`, `StoredEntry`, `WatchEvent`) |
`history.rs` (event log + compaction watermark, private, embedded) |
`key.rs` (`Key`, `KeyPrefix`) | `error.rs` (`StorageError`). Re-exported
at the crate root.

## Status / next

- Landed: trait + `EmbeddedStorage` + the RocksDB backend (T2.1 + T2.2 +
  T2.3 all done in the SSOT; Q33 since Sprint 25).
- Parity gate: `crates/storage/tests/contract/` — 26 portable contract
  cases instantiated for BOTH backends via `storage_contract!` (1
  embedded-only history-eviction case stays in `embedded_storage.rs`).
  The rdb side reports `rdb_storage` 31 (26 contract + 4 file-backed
  durability: reopen / revision-monotonic / watch-replay /
  compaction-watermark + a double-open exclusion test).
- Durability gate: `scripts/durability-e2e.sh` D1-D4 in CI, now on
  `rdb://` (restart persistence with identical resourceVersions,
  revision continuity + watch replay across restart, controller resync
  smoke).
- Default backend is still `EmbeddedStorage`; `rdb://` is opt-in
  (durable-by-default is a possible future decision, not taken). Open
  tails: policy-grade retention/compaction scheduling; the real
  etcd-gRPC client now lives at T3.4 (HA).
