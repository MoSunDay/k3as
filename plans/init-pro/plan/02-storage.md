# Layer 2 — Storage

Mirrors `index.md` TODO IDs **T2.1–T2.3**. Provides the durable backend the
APIServer (T1.2) sits on. Per Q1, etcd is **bundled into the single binary**
(embed/FFI/subprocess), never a separate download.

---

## T2.1 — etcd embed/FFI/子进程 bundling

- **目标 / Goal**
  Run etcd from inside the single `init-pro` binary (no separate etcd process
  the user must install).

- **核心实现 / Core implementation**
  - Preferred: `etcd` Go library linked via FFI/cgo-free binding, or a
    Rust re-binding of the embedded-server API (investigate `etcd-3.5`
    `embed.Etcd` exposure).
  - Fallback: bundle the `etcd` Go binary (T0.2) and supervise it as a
    child (k3s model), reexec via T0.1 multicall as `init-pro etcd`.
  - Single-node bootstrap + cluster-join flags (k3s `--cluster-init`,
    `--server`).
  - Lifecycle owned by `init-pro server` supervisor (T0.3 shutdown).

- **验收手段 / Acceptance**
  - `init-pro server` brings up etcd; `init-pro etcdctl endpoint health` OK.
  - Crash test: kill inner etcd, supervisor restarts within deadline.

- **状态 / Status** — done
- **证据 / Evidence** — Sprint 10: T2.1 **spike decision** recorded — implement etcd *semantics* in pure Rust behind the `StorageBackend` trait rather than FFI-linking Go etcd or supervising a bundled subprocess. The embedded backend (`crates/storage/src/embedded.rs`) is the zero-dependency default; the real etcd-gRPC client remains an alternative trait impl. `Action::Etcd` multicall alias still a peer stub until the subprocess/embed path is needed for HA (T3.4).
- **卡点 / Blockers** — resolved the FFI-vs-subprocess trade-off by choosing pure-Rust semantics; real etcd backend deferred (not on the critical path until HA/T3.4).
- **依赖 / Depends on** — T0.2

---

## T2.2 — etcd v3 数据面 = APIServer storage backend

- **目标 / Goal**
  APIServer storage backed by etcd v3 semantics: watch with historical
  replay, compaction, and `resourceVersion` behavior identical to upstream.

- **核心实现 / Core implementation**
  - Backend contract = `StorageBackend` trait (Q17): the embedded pure-Rust
    impl is the v1 default; a real etcd v3 gRPC client remains an
    alternative trait impl (deferred to T2.3 — not needed on the critical
    path).
  - Key layout `/registry/...` (upstream parity).
  - Transactions for CAS on `resourceVersion` (`if_revision`).
  - Watch: bounded revision event log (`src/history.rs`, default 10k
    revisions) — `watch(prefix, start_revision)` replays retained history
    (etcd-inclusive semantics) then continues live from a single
    lock-ordered seam; lossless and duplicate-free by construction.
    Lagging consumers get the stream closed (re-list + re-watch), never a
    silent skip.
  - Compaction: `compact(revision)` on the trait advances the watermark
    (embedded impl clamps future revisions to current); reads unaffected.
    A watch start at/below the watermark → `StorageError::Compacted`
    (etcd `ErrCompacted`), surfaced by the apiserver as `410 Gone` /
    reason `Expired`.
  - Leader election does NOT use etcd TTL leases: Q18 locks
    `coordination.k8s.io` Lease objects + `resourceVersion` CAS instead
    (backend-agnostic, upstream client-go semantics).
  - `DELETED` watch events carry the object's final state (upstream
    parity; required by the future GC controller, T3.1).

- **验收手段 / Acceptance**
  - T0.6 golden storage cases pass (CRUD + optimistic-concurrency
    conflict on stale `resourceVersion` + watch replay, G15).
  - Watch from `resourceVersion=N` yields exactly the events after N
    (k8s semantics; apiserver maps to etcd start `N+1`).

- **状态 / Status** — done
- **证据 / Evidence** — Sprint 10: trait + `EmbeddedStorage` (etcd `KeyValue` parity, `/registry/...` layout) + 15 integration tests. Sprint 10.5: wired into REST CRUD/watch (T1.2b). Sprint 12: watch historical replay + compaction closeout — `crates/storage/src/history.rs` (event log + watermark, 4 unit tests), replay-then-live seam under one lock, `compact` on the trait, `Compacted` → 410 Gone mapping in the apiserver, k8s↔etcd `resourceVersion` translation fixed (`ListParams` was silently dropping the wire param). 26 storage integration tests (11 new: replay order/ seam losslessness under concurrent writes / prefix filtering / future start / compacted errors / eviction / prev-object-on-delete / multi-watcher) + 4 unit; 4 new apiserver watch-replay tests over real TCP; 360 total, golden 15/15 (G15).
- **卡点 / Blockers** — none for v1 scope. Durability (restart persistence), policy-grade retention/compaction scheduling, and the real etcd-gRPC client are T2.3 (Q17). Policy note: the 10k-revision default retention is deliberately generous; aggressive compaction would starve slow informers.
- **依赖 / Depends on** — T2.1, T1.1

---

## T2.3 — SQLite/KINE 兼容替代后端

- **目标 / Goal**
  A non-etcd backend (SQLite via KINE-equivalent) for the single-node /
  embedded flavor — k3s `--datastore-endpoint` parity.

- **核心实现 / Core implementation**
  - KINE-style abstraction: a generic `StorageBackend` trait
    (`Watch`/`List`/`Create`/`Update`/`Delete`) with etcd and SQLite impls.
  - SQLite (via `rusqlite`/`sqlx`) with a `kv` table + revision column;
    change feed for watch.
  - Selected when `--datastore-endpoint` is a SQLite DSN or `--disable-etcd`.

- **验收手段 / Acceptance**
  - Same T0.6 golden storage cases pass on SQLite backend.
  - Doc test: switch backend via flag only, no code change.

- **状态 / Status** — not-started
- **证据 / Evidence** — —
- **卡点 / Blockers** — SQLite watch latency / correctness under load.
- **依赖 / Depends on** — T2.2
