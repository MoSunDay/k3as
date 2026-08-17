# storage-layer

The resource storage layer (T2.1/T2.2/T2.3, all done). Lives in
`crates/storage/`.

## What it is

Defines the generic `StorageBackend` trait (`Watch` / `List` / `Create` /
`Update` / `Delete`) over the upstream `/registry/...` key layout, plus TWO
interchangeable backends with identical etcd-faithful semantics: the
embedded in-process store (`EmbeddedStorage`, zero external dependencies,
the default + test double) and a durable libsql/SQLite backend
(`SqliteStorage`, T2.3/Q29) selected by `--datastore-endpoint
sqlite://<path>`.

## T2.1 spike decision

T2.1 asked to choose between (a) linking Go's etcd via FFI or (b)
supervising a bundled etcd subprocess (the k3s model). Both carry heavy
Go<->Rust integration cost. For a from-scratch Rust distribution we
instead implement the etcd SEMANTICS in pure Rust behind the trait. The
SQLite/KINE backend landed as T2.3 (Q29: libsql 0.9 core-only, hard
requirement); the real etcd gRPC client is re-scoped to T3.4 (HA
multi-server, its only consumer — Q17 supersede note). Backend selection
is `--datastore-endpoint` only; empty/None keeps the embedded default.

## Semantics (etcd-faithful, both backends)

- A single monotonic cluster-wide `Revision`; every successful write bumps
  it.
- Per key: `create_revision`, `mod_revision` (== Kubernetes
  `resourceVersion`), and `version` (write count since creation) -
  mirrors etcd's `KeyValue`. The SQLite backend stores `version` as an
  explicit column (create=1, update=prev+1, tombstone carries the final
  version) so compaction cannot alter it.
- Optimistic concurrency: `update`/`delete` take an `if_revision` CAS
  (`kubectl apply` stale-revision conflicts).
- Watch: live events via tokio broadcast; `watch(prefix, Some(n))` REPLAYS
  retained history from revision `n` (etcd-inclusive) before continuing
  live -- one lock-ordered seam, no gaps/dups. Embedded: bounded ring,
  default 10k revisions (Sprint 12, T2.2). SQLite: replay is SQL
  (`SELECT id >= start ORDER BY id` over the append-only `kv` event
  table), so it survives restart; broadcast is sent strictly post-COMMIT
  under one connection mutex (Sprint 19, T2.3).
- Compaction: `compact(rev)` advances the watermark; a watch start at/below
  it errors `Compacted` (upstream 410 Gone / Expired). SQLite keeps the
  latest row per key and persists the watermark to `meta`.
- `DELETED` watch events carry the object's final state (`prev`).
- Leader election does NOT use this layer's leases (none exist): see Q18
  (coordination.k8s.io Lease + resourceVersion CAS).

## Module map (crates/storage/src/)

`backend.rs` (`StorageBackend`, `Watch`) | `embedded.rs`
(`EmbeddedStorage`) | `sqlite/` (`mod.rs` `SqliteStorage` on libsql
core-only + `schema.rs` kine-style `kv`/`meta` DDL + `watch.rs` replay +
`tests.rs`) | `entry.rs` (`Revision`, `StoredEntry`, `WatchEvent`) |
`history.rs` (event log + compaction watermark, private, embedded) |
`key.rs` (`Key`, `KeyPrefix`) | `error.rs` (`StorageError`). Re-exported
at the crate root.

## Status / next

- Landed: trait + `EmbeddedStorage` + the SQLite backend (T2.1 + T2.2 +
  T2.3 all done in the SSOT; Q29).
- Parity gate: `crates/storage/tests/contract/` — 26 portable contract
  cases instantiated for BOTH backends via `storage_contract!` (1
  embedded-only history-eviction case stays in `embedded_storage.rs`).
  Storage crate reports lib 12 (4 history + 8 sqlite unit),
  `embedded_storage` 27, `sqlite_storage` 31 (26 contract + 5
  file-backed durability: reopen/revision-monotonic/watch-replay/
  compaction-watermark/WAL).
- SQLite tx hardening (post-review, pre-submit): a write future dropped
  between BEGIN/COMMIT, or a failed COMMIT, no longer poisons the single
  connection — next write/compact self-heals via `is_autocommit()` +
  ROLLBACK; replay clamps `Revision > i64::MAX` instead of wrapping
  negative (would have replayed the whole log). Pinned by unit +
  contract tests.
- Durability gate: `scripts/durability-e2e.sh` D1-D4 in CI (restart
  persistence with identical resourceVersions, revision continuity +
  watch replay across restart, controller resync smoke).
- Default backend is still `EmbeddedStorage`; `sqlite://` is opt-in
  (sqlite-by-default is a possible future decision, not taken). Open
  tails: policy-grade retention/compaction scheduling; the real
  etcd-gRPC client now lives at T3.4 (HA).
