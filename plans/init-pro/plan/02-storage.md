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
  APIServer storage backed by etcd v3 with leases, compaction, and
  `resourceVersion` semantics identical to upstream.

- **核心实现 / Core implementation**
  - etcd v3 gRPC client (Rust `etcd-client` or tonic-generated).
  - Key layout `/registry/...` (upstream parity).
  - Transactions for CAS on `resourceVersion`; watch multiplexing.
  - Compaction policy + TTL leases for LeaderElection/locks.

- **验收手段 / Acceptance**
  - T0.6 golden storage cases pass (CRUD + optimistic-concurrency
    conflict on stale `resourceVersion`).
  - `etcdctl get /registry/pods --prefix` shows expected layout.

- **状态 / Status** — in-progress
- **证据 / Evidence** — Sprint 10: `crates/storage/` landed — `StorageBackend` trait (`Watch`/`List`/`Create`/`Update`/`Delete` + `if_revision` CAS) and `EmbeddedStorage` (etcd `KeyValue` parity: create_revision/mod_revision/version; `/registry/...` upstream key layout). 15 integration tests green: CRUD round-trip, resourceVersion monotonicity, optimistic-concurrency conflict on stale revision, list prefix/namespace filtering, watch put/delete events.
- **卡点 / Blockers** — embedded backend wired into REST (T1.2b, Sprint 10.5); real etcd-gRPC backend deferred to T2.3; embedded backend is live-watch only (no historical replay) and in-memory/non-durable until T2.3.
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
