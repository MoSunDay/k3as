# Layer 2 — Storage

Mirrors `index.md` TODO IDs **T2.1–T2.3**. Provides the durable backend the
APIServer (T1.2) sits on. Per Q1, etcd is **bundled into the single binary**
(embed/FFI/subprocess), never a separate download.

---

## T2.1 — etcd 语义数据面（纯 Rust 实现，Q17）

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
    alternative trait impl (re-scoped to T3.4 — its only consumer is HA
    multi-server; see the Q17 supersede note + Q29).
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
- **卡点 / Blockers** — none for v1 scope. Durability (restart persistence) is delivered by T2.3 (`RdbStorage`, opt-in via `--datastore-endpoint rdb://<path>`, Q33); the real etcd-gRPC client is re-scoped to T3.4 (Q17 supersede note). Policy-grade retention/compaction scheduling remains open. Policy note: the 10k-revision default retention is deliberately generous; aggressive compaction would starve slow informers.
- **依赖 / Depends on** — T2.1, T1.1

---

## T2.3 — rdb/RocksDB 持久化后端 (kine 语义, Q33)

> **Sprint 25 (Q33) 重写：** 实现从 libsql/SQLite（Sprint 19, Q29）换底为
> rdb/RocksDB（`RdbStorage`）。原 `crates/storage/src/sqlite/`（mod/schema/
> watch/tests）与 `crates/storage/tests/sqlite_storage.rs` 已整体删除，libsql
> 依赖从根 + storage `Cargo.toml` 移除；`sqlite://` DSN 现返回
> `StorageError::Unsupported`（指向 `rdb://`）。历史证据保留在本卡末尾。

- **目标 / Goal**
  A non-etcd backend for the single-node / embedded flavor — k3s
  `--datastore-endpoint` parity — that is **durable across restarts**
  (entries, revisions, watch history, compaction watermark). As of
  Sprint 25 the durable option is **rdb/RocksDB（kine 语义）**.

- **核心实现 / Core implementation**
  - `crates/storage/src/rdb_backend/`（`mod.rs` + `keys.rs` + `watch.rs`）：
    `RdbStorage` 跑在 **rdb crate（RocksDB）** 的 `store` engine 上（根
    `Cargo.toml` path dep `link_repos/rdb`, `default-features = false,
    features = ["store"]`, Q33）。
  - 单 CF 内按前缀分区的 kine 式 append-only 记录模型：`meta/revision`
    （cluster revision, 8 字节大端 u64）、`meta/compact`（compaction
    watermark, 最低保留 log rev）、`obj/<key>`（live `StoredEntry` JSON,
    删除即缺席）、`log/<rev 大端>`（每次成功写入一条 `LogRecord` JSON）。
    `log/` 定宽大端 → 字典序 == revision 序，range scan 即 revision 序回放；
    `obj/` 与 `log/` 分离（SQLite 时代 row 表即 log 表）。
  - 写路径：一次写入 = 一个原子 WriteBatch（`obj/` upsert/delete +
    `log/<rev>` append + `meta/revision` bump），批量落盘并 fsync；
    回放/compaction 语义复刻自原 sqlite 写路径（etcd revision/CAS/watch
    历史回放/compaction watermark 全量保留）。`RdbStorage::open(&Path)`
    为 async。
  - 选择：`--datastore-endpoint`（crates/cli/src/runtime.rs）— 空/None →
    `EmbeddedStorage` 默认不变（Q17；仅显式 opt-in）；`rdb://<path>` →
    `RdbStorage`（`Dsn::Rdb(PathBuf)` 分类, 启动日志行含
    "rdb datastore (rocksdb) — T2.3/Q33", durability-e2e grep 该行）；
    `sqlite://` → `Unsupported`（Sprint 25 破坏性 DSN 变更）；其他 scheme
    fatal。`--kine-tls`/`datastore-*` 保持 noop（flag-matrix §C.6）。

- **验收手段 / Acceptance**
  - Contract parity: `crates/storage/tests/contract/` 的 26 个可移植用例
    继续通过 `storage_contract!` 对两个后端各跑一遍；新
    `crates/storage/tests/rdb_storage.rs` = 26 contract + 4 durability
    （reopen 保 entries+revisions / revision 严格单调 / watch 跨重启回放 /
    compaction watermark 存活）+ double-open 排他测试。
  - `scripts/durability-e2e.sh` D1–D4（`rdb://` 上）：boot+seed / SIGTERM
    drain / 重启持久化（resourceVersion 逐一相同）/ revision 计数续走
    （D4 实测 15→16）+ 跨重启 watch 回放 + controller resync smoke
    （CI gate）。

- **状态 / Status** — done
- **证据 / Evidence** — Sprint 25 (Q33): `RdbStorage` + `rdb_storage.rs`
  (26 contract + 4 durability + double-open) 交付; durability D1–D4 4/4 在
  `rdb://` 上全绿; `scripts/local-up.sh` boots
  `rdb://$DD/server/state.rdb`; workspace 852 tests green, clippy
  `-D warnings` 0. 历史: Sprint 19 的 libsql/SQLite 实现（`SqliteStorage`,
  `sqlite_storage` 31 tests, Q29）已被本卡顶部的删除说明取代。
- **卡点 / Blockers** — none. Default backend stays `EmbeddedStorage`
  (flipping to durable-by-default is a possible future decision, NOT
  taken); the real etcd-gRPC client is re-scoped to T3.4 (Q17 supersede
  note).
- **依赖 / Depends on** — T2.2
