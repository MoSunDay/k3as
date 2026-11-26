# Layer 6 — Addons / Manifests

Mirrors `index.md` TODO IDs **T6.1–T6.3**. Q2-compatible: addons ship as
standard manifests/CRDs that `kubectl`/`helm` see natively. Parity with
k3s `pkg/deploy/` auto-deploy machinery.

---

## T6.1 — manifest 自动部署 (HelmChart auto-deploy)

- **目标 / Goal**
  Auto-deploy manifests dropped into the data dir + a `HelmChart` CRD that
  init-pro reconciles (k3s `pkg/deploy/controller.go` parity).

- **核心实现 / Core implementation**
  - Controller watches `<data-dir>/server/manifests/*.yaml` and applies on
    change (k3s deploy/ pkg model).
  - `HelmChart` CRD (k3s `helm.cattle.io/v1`); bundled `helm` (multicall
    alias) renders charts server-side.
  - `--disable` gating (T0.4) for the standard chart set.

- **验收手段 / Acceptance**
  - Golden: drop a manifest → it appears via `kubectl get`; create a
    `HelmChart` → its resources render.

- **状态 / Status** — not-started
- **证据 / Evidence** — —
- **卡点 / Blockers** — none
- **依赖 / Depends on** — T1.2, T3.1

---

## T6.2 — 标准 addon (T6.2a 内嵌 Cluster DNS + T6.2b local-path; metrics-server 弃置)

- **目标 / Goal**
  The k3s default addon stack, toggle-able via `--disable` (T0.4):
  内嵌 Cluster DNS + local-path 供给（`servicelb`/`traefik` 已由
  Layers 5/4.3 取代; metrics-server 按 Q59 S49 决策显式弃置于前向支持子集外——HPA/`kubectl top`
  若成为目标生态负载, 按 Q59 addition path 重入; 台账 phase-4-closure.md §6）。

- **核心实现 / Core implementation**
  - **T6.2a — 内嵌 Cluster DNS (S43 排期, Q45 草案)**:
    `--cluster-dns`/`--cluster-domain` 从 noop 转 wired；CRI
    `PodSandboxConfig` 增 `dns_config` 字段下发 pod resolv.conf；DNS
    服务端纯 Rust 内嵌（service.namespace.svc.cluster.local A 记录 ←
    Service/Endpoints watch；headless 答 endpoint IP 集；其余上游
    forward 至宿主 resolv.conf）。宿主进程仿 Q34 "flannel 内嵌 agent"
    先例定夺（倾向 kubelet 内嵌模块，Q45 定）。明确 out: SRV/PTR/pod
    DNS。
  - **T6.2b — local-path 供给 (S44 排期, Q46 草案, 修订 Q22)**:
    `schema.rs` 一行式注册 PersistentVolume、StorageClass
    （k8s_openapi 原生类型，注册即得全套 generic CRUD/watch/SSA）；
    provisioner+binder 控制器（PVC Pending → 在 data-dir 下造
    hostPath PV → Bound；默认 StorageClass 注解）。明确 out: 多节点
    亲和调度、VolumeBindingMode 延迟绑定。
  - metrics-server: **显式弃置** (S49, Q59 注记)——前向支持子集外,
    addition path 重入; 不再排队。

- **验收手段 / Acceptance**
  - T6.2a: wire 格式/压缩单测 + 新 `scripts/dns-e2e.sh` D1–D5（pod 内
    getent 服务名解析并连通 ClusterIP，依赖 T4.3 Scope C）。
  - T6.2b: reconcile 状态机单测 + 新 `scripts/storage-e2e.sh` V1–V3。

- **状态 / Status** — not-started
- **证据 / Evidence** — —
- **卡点 / Blockers** — T6.2a 依赖 T4.3 Scope C ClusterIP 承载 (S42)
- **依赖 / Depends on** — T6.1, T4.3

---

## T6.3 — 内嵌 Docker Registry v2 镜像仓库 (tag 保留策略)

- **目标 / Goal**
  An image registry embedded in the `init-pro server` process — no
  external install, no docker daemon — speaking the Docker Registry HTTP
  API v2 (`/v2/`, `_catalog`, `tags/list`, manifests, blobs + resumable
  uploads) so `docker`/`containerd`/`crictl` work unmodified. On top of
  the protocol: tag management — per repository keep the newest N tags
  (default 10) while keep-forever patterns are excluded from the count
  and never pruned (Q40).

- **核心实现 / Core implementation**
  - New crate `crates/registry` (axum 0.8 over the workspace deps;
    `#![forbid(unsafe_code)]`, pure-function policy math, no classes):
    `validate.rs` (reference grammar, name/tag/digest), `store.rs`
    (global blob CAS + per-repo manifests/tags; stream hashing),
    `policy.rs` (backtracking `*` glob matcher + pure `prune_plan`),
    `gc.rs` (retention orchestration: tag -> manifest ->
    unreferenced-blob sweep with a 600s freshness grace), `api/`
    (`mod.rs` router + plumbing, `manifest.rs` / `blob.rs` handlers) +
    `uploads.rs` (V2 wire surface; right-first marker path parsing
    because axum catch-alls must be terminal). Blob GET streams the CAS
    file with an explicit `Content-Length` (never buffered whole);
    manifest digests pass a `sha256` grammar gate (400
    `MANIFEST_INVALID`) before any filesystem probe; shutdown drain is
    bounded at 10 s (listener aborted with a warn on timeout).
  - Storage layout `<data_dir>/registry/`: `blobs/sha256-<hex>` (global
    CAS, cross-repo mount is a no-op copy), `repos/<name>/manifests/`
    (per-repo by digest), `repos/<name>/tags/<tag>.json`
    (`{digest, mediaType, pushed_at, seq, size}`), `uploads/<uuid>`,
    `keep.json`.
  - Retention runs on every manifest push: newest-N by `(pushed_at,
    seq)`, keep-forever patterns (CLI `--registry-keep-tag` union hot-
    reloaded `keep.json`, `repo:tag` with `*` wildcards) excluded from
    the count. `--registry-tag-retention 0` = unlimited.
  - Server wiring mirrors anyhome (Q36): default-on, `--registry-addr`
    (default `127.0.0.1:15000`; 5000 collides with macOS AirPlay),
    `--disable-registry`, graceful drain in the supervised shutdown
    sequence. Referential validation on push (`MANIFEST_BLOB_UNKNOWN`,
    after the 400 `MANIFEST_INVALID` digest grammar gate); delete-by-tag
    refused (`DIGEST_INVALID`), delete-by-digest drops the manifest +
    its tags and GCs blobs, and an unknown digest answers 404
    `MANIFEST_UNKNOWN` (202 only when something was removed).

- **验收手段 / Acceptance**
  - 18 tests total (12 unit + 6 HTTP integration over a real socket).
  - Unit: grammar, glob/prune_plan (newest-N, keep-forever excluded,
    zero=unlimited, deterministic ties), query decoding, path targets.
  - HTTP integration (`crates/registry/tests/v2_http.rs` and
    `v2_manifest.rs`, real socket): ping/catalog, monolithic + resumable
    + mount uploads, manifest push/pull with `Docker-Content-Digest`,
    referential 404, malformed-digest 400s, a 2 MiB streamed blob GET,
    12-push retention (10 survive), keep.json pin survives and is not
    counted, delete semantics (incl. unknown digest 404).
  - `scripts/registry-e2e.sh` R1–R6 against a booted server (offline
    fabricated docker-v2 image, curl push, retention + keep.json +
    delete) — default 7 passed / 0 failed / 1 skipped (R7 real
    `crictl pull` env-gated); R7 recorded a real 8 passed / 0 failed /
    0 skipped run with a live containerd in Sprint 34.

- **状态 / Status** — done (Sprint 33; Sprint 34 hardening, Q40)
- **证据 / Evidence** — `crates/registry/` (18 tests); `scripts/registry-e2e.sh`
  default 7 passed / 0 failed / 1 skipped, R7 real 8 passed / 0 failed /
  0 skipped with a live containerd; `scripts/cli-flag-parity-test.sh`
  20/0 with `live_help t-registry`; CHANGELOG Sprint 33 + 34.
- **卡点 / Blockers** — none
- **As-built (长稳清扫 2026-11-26, A1/B7/B9)** — A1 (commit 13e26a2)
  `docker save` 导入流式化: `tarstream.rs` (NEW) 逐 layer 落盘
  hash-while-copy (256 KiB 窗口, gzip/zstd 嗅探), `ImportLimits` (默认
  8 GiB 镜像 / 4 MiB 元数据, `INIT_PRO_PIPELINE_MAX_IMAGE_BYTES`), push
  `BlobBody::File` 从盘流式 PATCH 并复核 digest。B7 push 客户端 deadline
  (`PUSH_TIMEOUT` 120 s 整次预算 / `CONNECT_TIMEOUT` 5 s / 响应体 32 MiB
  上限)。B9 upload 卫生: PATCH 失败 unlink 半成品;
  `sweep_stale_uploads` (`STALE_UPLOAD_SECS` 3600, 每轮 ≤1024 项) 随 boot
  与每次 retention 执行。
- **依赖 / Depends on** — T0.3
