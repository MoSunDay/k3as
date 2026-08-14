# init-pro — Index (SSOT)

This file is the **single source of truth** for all 33 TODOs. The
`plan/<layer>.md` files mirror the same TODO IDs (same content, per-layer
grouping for easier editing). **Edit both in lock-step.**

Legend & rules: see `README.md` §6 and `template/TODO.md`.

## Status table

| ID | Layer | Title | Status | Depends on |
|----|-------|-------|--------|------------|
| T0.1 | 0 | 单一二进制 multicall crate 骨架 | done | — |
| T0.2 | 0 | 构建与 bundling pipeline | done | T0.1 |
| T0.3 | 0 | 公共基础设施 crate (log/config/signal) | done | T0.1 |
| T0.4 | 0 | CLI: multicall + k3s 兼容 flag | done | T0.1, T0.3 |
| T0.5 | 0 | 规划体系与文档中枢 | done | — |
| T0.6 | 0 | 协议兼容性测试基线 (golden conformance) | done | T0.1 |
| T1.1 | 1 | 资源模型与 API group schema | done | T0.3 |
| T1.2 | 1 | APIServer 核心 (REST + kubectl 真实交互) | done | T1.1, T2.2 |
| T1.3 | 1 | 认证授权 (kubeconfig/token/RBAC) | not-started | T1.1 |
| T2.1 | 2 | etcd embed/FFI/子进程 bundling | done | T0.2 |
| T2.2 | 2 | etcd v3 数据面 = APIServer storage backend | done | T2.1, T1.1 |
| T2.3 | 2 | SQLite/KINE 兼容替代后端 | not-started | T2.2 |
| T3.1 | 3 | kube-controller-manager 等价核心循环 | done | T1.2, T2.2 |
| T3.2 | 3 | 调度器 (kube-scheduler 等价 + extender seam) | not-started | T1.2, T4.1 |
| T3.3 | 3 | bootstrap / 证书 / token 轮转 | not-started | T1.3, T2.2 |
| T3.4 | 3 | HA 多 server (etcd 集群 + 选举) | not-started | T2.2, T3.3 |
| T4.1 | 4 | containerd bundling 与 CRI 实现 | not-started | T0.2 |
| T4.2 | 4 | kubelet 等价 (pod 生命周期 + status) | not-started | T4.1, T3.2 |
| T4.3 | 4 | CNI/网络 (flannel 等价 + ServiceLB L4) | not-started | T4.2 |
| T4.4 | 4 | 网络策略 (netpol 等价) | not-started | T4.3 |
| T4.5 | 4 | 节点注册/心跳/代理隧道 | not-started | T4.2, T1.2 |
| T5.1 | 5 | mlua + coroutine↔async 桥 | done | T0.3 |
| T5.2 | 5 | HTTP 管线 + phase hooks | done | T5.1 |
| T5.3 | 5 | resty::* 等价标准库 | done | T5.1, T5.2 |
| T5.4 | 5 | 内置 Router 核心 + Ingress→Lua 路由编译 | done | T5.2, T5.3, T1.1 |
| T5.5 | 5 | 热加载 / 动态配置 (no-restart reload) | not-started | T5.4 |
| T5.6 | 5 | ServiceLB (L4/LB 数据面) | not-started | T5.4, T4.3 |
| T5.7 | 5 | 内置 Router 作为平台配置变量 | not-started | T5.4, T4.5 |
| T6.1 | 6 | manifest 自动部署 (HelmChart auto-deploy) | not-started | T1.2, T3.1 |
| T6.2 | 6 | 标准 addon (CoreDNS/local-storage/metrics-server) | not-started | T6.1 |
| T7.1 | 7 | AI Agent workload CRD + scheduler extender | not-started | T3.2, T1.2 |
| T7.2 | 7 | GPU/资源调度与策略 | not-started | T7.1, T4.2 |
| T7.3 | 7 | argo-workflows 最小化整合 (Workflow/CronWorkflow 一级公民内置) | not-started | T1.2, T3.1 |

**Counts:** 33 TODOs · Layer0=6 · Layer1=3 · Layer2=3 · Layer3=4 · Layer4=5 · Layer5=7 · Layer6=2 · Layer7=3.

---

## DAG (critical path)

```
T0.1 ─┬─> T0.2 ─┬─> T2.1 ─> T2.2 ─┬─> T1.2 ─┬─> T3.1 ─> T6.1 ─> T6.2
      │         │                 │          ├─> T3.2 ─> T7.1 ─> T7.2
      ├─> T0.3 ─┤                 │          ├─> T3.3 ─> T3.4
      │         └─> T4.1 ─> T4.2 ─┼─> T4.3 ─> T4.4
      ├─> T0.4                      │          └─> T4.5
      ├─> T7.3 <─ T3.1 <─ T1.2      │   (argo-workflows first-class)
      └─> T0.6 (golden gate,        │
                every TODO must     T1.1 ─┬─> T1.3
                keep it green)      T2.3 <─┘
                                         T5.1 ─> T5.2 ─┐
                                         T5.3 <───────┘─> T5.4 ─┬─> T5.5
                                                                  ├─> T5.6
                                                                  └─> T5.7
```

- **Critical path (platform):** T0.1 → T0.2 → T2.1 → T2.2 → T1.2 → T3.1/T3.2
  → T4.2 → end-to-end cluster (M3).
- **De-risk path (Q5):** T0.1 → T0.3 → T5.1 → T5.2 → T5.4 (M1 spike).
- **T0.6** is a gate, not a node: any TODO merging must keep it green.
- **Sprint 14 — T3.1b: controller-manager 收官 (T3.1 → done).** Six slices: (S1) Deployment rolling update — `maxSurge`/`maxUnavailable` pacing read off raw JSON specs (upstream defaults 25%/25%), NewReplicaSetAvailable/Progressing condition transitions in `controllers/rollout.rs` + `conditions.rs`; (S2) `kubectl rollout status` (**Q21**: pure `evaluate` fn + 250ms poll loop, exit-code parity 0/1); (S3) StatefulSet controller (**Q22**: ordinal identity, per-claim PVC objects + ControllerRevisions as real objects, OrderedReady/Parallel, RollingUpdate/OnDelete); (S4) DaemonSet controller (Node list as source of truth, nodeSelector + first nodeAffinity term placement, status numbers); (S5) GC + namespace lifecycle (**Q20**: cache-verified owner sweep + annotation-marked Orphan + namespace drain/finalize, injected `NowFn` clock in `common/src/time.rs`); (S6) golden G18–G21 → **21/21**. Two hard bugs fixed en route: `WorkQueue::done()` released the dirty lock before removing from `processing` (concurrent `add` could strand a key forever — now atomic; regression test `concurrent_add_during_done_never_loses_keys`), and apiserver PUT stored bodies verbatim so namespace-less PUTs emitted watch events without namespace, desyncing informer caches into silent no-op reconciles (PUT now defaults/validates `metadata.namespace`; informer additionally normalizes event namespace from the storage path). Unlocks T6.1/T7.3 on the DAG. 489 tests (+90 over Sprint 13; breakdown in CHANGELOG), clippy/fmt clean.
- **Sprint 13 — T3.1a: controller-manager core loops (first slice of T3.1).** New `crates/controllers` (15 src modules + 3 test files, all ≤334 lines): the framework — `Client` trait + in-process `StorageClient` over the shared `Arc<dyn StorageBackend>` (**Q19**), latched `Stop` token, client-go-semantics `WorkQueue` (dirty/processing dedup, rate-limited requeue, forget), `Informer` (LIST→cache→watch-from-max_revision, re-list on lag/close, synthetic initial events) + `ObjectStore` for sync-handler reads, JSON object helpers (label selectors, controller_of, semantic_eq write-if-changed guard, fnv template hash, RFC3339), Lease+CAS leader election (Q18, injected `NowFn` clock for deterministic tests), and `ControllerManager::spawn` (leader-gated; 4 informers deployments/replicasets/pods/services, 3 workqueues, 2 workers each, pod→owner-RS + matching-Service reverse enqueue, default/kube-system/kube-public/kube-node-lease namespace bootstrap). Reconcilers: ReplicaSet (count owned pods, 5-char suffix names, delete surplus preferring unscheduled), Deployment (template-hash-named RS; instant scale-up + drain-down = Recreate-flavored v1 rollout, maxSurge rolling is T3.1b; old RS deleted at status.replicas==0; status write-if-changed), Endpoints (selector matching both LabelSelector and plain-map shapes; ready = Ready cond True or NO conditions — documented v1 default without kubelet; placeholder deterministic 10.42.x.y pod IPs from UID hash until T4.2/T4.3). Wired: `cli/discovery.rs` serves apps/v1 (Deployment/ReplicaSet + inert StatefulSet/DaemonSet schema for T3.1b) + core/v1 endpoints + coordination.k8s.io/v1 leases; `cli/runtime.rs` spawns the manager in the apiserver branch sharing the same storage Arc and drains it on shutdown (disable-apiserver ⇒ no controllers). 399 tests green (+39: 25 unit inline + 14 integration — informer 4, leaderelection 4, controllers 6 incl. quiesce-after-convergence anti-oscillation gate); golden 17/17 (new G16 apps/v1 byte diff, G17 real-binary Deployment scale 1→3→1 convergence + Endpoints membership). Q19 recorded (in-process transport; HTTP client swaps in at the trait boundary with T3.4/T1.3). T3.1 → in-progress (T3.1a done); deferred to T3.1b: StatefulSet, DaemonSet, GC, `kubectl rollout status` parity. Next gates: T3.1b completion, then T3.2 (scheduler); T4.1 early risk-spike still recommended.
- **Sprint 12 — T2.2 closeout: watch historical replay + compaction.** The embedded backend now bridges the informer disconnect window: `watch(prefix, start_revision)` replays retained history (ring buffer, default 10k revisions, `crates/storage/src/history.rs`) from the etcd-inclusive start revision and continues with live events from a single lock-ordered seam (lossless, duplicate-free); a start at/below the compaction watermark surfaces as `StorageError::Compacted` → HTTP `410 Gone`/`Expired`. `compact(rev)` added to the trait (embedded impl clamps future revisions to current; get/list unaffected). Fixed along the way: `ListParams.resource_version` was missing its serde rename so the wire `resourceVersion` was silently dropped (every watch was live-only); watch now maps k8s "events after N" → etcd start `N+1`, `"0"` → start of retained history. `DELETED` watch events now carry the object's final state (was a name-only stub). Leader election re-scoped off etcd leases: Q18 locks `coordination.k8s.io` Lease + resourceVersion CAS (upstream client-go semantics, backend-agnostic). 360 tests green (+19: 11 storage watch/compaction integration, 4 history unit, 4 apiserver watch-replay); golden 15/15 (new G15: watch replay). T2.2 → done; next critical-path gate: T3.1.
- **Sprint 10 — storage layer + releasable increment.** T2.1/T2.2 started: the `StorageBackend` trait (Watch/List/Create/Update/Delete + `if_revision` CAS) and an embedded, zero-dependency backend (`EmbeddedStorage`) with etcd-faithful revision + optimistic-concurrency semantics landed in `crates/storage/` (+15 integration tests). T2.1 spike **decision**: implement etcd *semantics* in pure Rust behind the trait rather than FFI-linking Go etcd or supervising a subprocess — the etcd-gRPC and SQLite/KINE impls (T2.2/T2.3) slot in as alternative trait impls via `--datastore-endpoint`. The golden fixtures were populated (T0.6 now fully green, 6/6). Repo-local memory bootstrapped (`agents.md` + `features/`); CI + a multi-stage `Dockerfile` added; workspace version bumped to 0.2.0.
- **Sprint 11 — Server-Side Apply field-manager (T1.2c).** `kubectl apply` now works end-to-end: a new `api::apply` module (`crates/api/src/apply/`: `field_set.rs` FieldsV1 extraction keyed by merge-key + `mod.rs` `apply_object` merge/conflict/prune + managedFields round-trip) is wired into the PUT/PATCH item handlers via `crates/apiserver/src/apply.rs` on Content-Type `application/apply-patch+yaml`. Semantics: 201 create-on-absent, 200 update with resourceVersion CAS, 409 conflict when a field is owned by another `fieldManager` (overridable with `?force=true` ownership steal), and pruning of fields the same manager no longer declares. JSON bodies only this sprint (the `+yaml` suffix is wire-compat). +15 tests (8 `crates/api/tests/apply.rs` + 7 `crates/apiserver/tests/rest_apply.rs`); 341 total green; golden harness G13/G14 → 14/14. T1.2c deferred → done; T1.2 → done (T1.2a discovery + T1.2b CRUD/watch + T1.2c SSA complete; admission-webhook seam is the documented no-op v1 default).
- **Sprint 10.5 — REST CRUD + watch over the embedded store (T1.2b).** The apiserver is no longer discovery-only: full REST CRUD handlers (create/list/get/replace/delete/patch) + a chunked `application/json` watch stream now back `crates/apiserver/` over `EmbeddedStorage`, wired via `api_app(registry, store, server_addr)`. Handlers implement k8s-faithful semantics: resourceVersion CAS on PUT/DELETE (409 Conflict on stale), strategic-merge/merge/JSON PATCH, namespace auto-injection, key-cursor pagination, and live-watch events (ADDED/MODIFIED/DELETED). Server-side apply field-manager deferred to T1.2c. 326 tests green (+15 apiserver); golden harness extended to 12/12 (G06 fixed: pods list now 200; G07–G12: CRUD round-trip + watch-open). T2.1 marked done (spike delivered + Q17 locked); T1.2 → in-progress (T1.2a discovery done, T1.2b CRUD+watch done, T1.2c SSA deferred).
- **Sprint 9 — Phase 1 closeout (M0 + M1 done).** T0.6 golden gate landed: `golden/` (4 byte-stable discovery fixtures) + `scripts/golden-conformance.sh` (boots a real server, 6/6 green empty-cluster baseline; the harness itself is the deliverable) + `.github/workflows/ci.yml` (`golden` required check). T5.3/T5.4 marked done — the M1 vertical slice (Ingress→route→real traffic, TLS/SNI, no-restart hot reload) is complete; remaining live `kube-rs` informer + dynamic Lua cert issuance deferred to T5.5/Phase 2. Phase 1 DoD (7/7) met. M0 (T0.1–T0.6) + M1 (T5.1–T5.4) = Phase 1.
- **Sprint 2 (T0.2 + T0.4): both done.** T0.4 = k3s CLI flag parity (17 wired + ~108 no-op + 7 conflict rules). T0.2 = full bundling pipeline: B1 acquire (3-mode, SHA-256 gate), B2 zstd embed codegen (content-addressed, level 19), B3 .sha256sums/.links dataverify manifests, B4 Q7 license gate + SPDX-2.3 SBOM, B5 runtime stage() (flock→write-tmp→dataverify→atomic-rename→symlink), B6 acceptance harness (8/8 green).

---

## Field reference (see `template/TODO.md`)

Each TODO carries 7 fields: **目标 / 核心实现 / 验收手段 / 状态 / 证据 / 卡点 / 依赖**.

Full detail for every TODO lives in `plan/00-foundation.md` …
`plan/07-agent-scheduling.md` (TODO IDs strictly mirror this file).
