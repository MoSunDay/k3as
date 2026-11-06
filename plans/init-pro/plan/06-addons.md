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

## T6.2 — 标准 addon (CoreDNS/local-storage/metrics-server)

- **目标 / Goal**
  The k3s default addon stack, toggle-able via `--disable` (T0.4):
  `coredns`, `local-storage`, `metrics-server` (and `servicelb`/`traefik`
  superseded by Layers 5/4.3).

- **核心实现 / Core implementation**
  - Bundled manifests pinned to upstream versions.
  - CoreDNS: standard Deployment + ConfigMap.
  - local-storage: a `StorageClass` + provisioner for node-local dirs.
  - metrics-server: upstream manifest; resource policy to run on control
    plane nodes.

- **验收手段 / Acceptance**
  - Golden: with defaults, DNS resolves, a PVC binds on `local-storage`,
    `kubectl top nodes` returns metrics; each `--disable` removes its piece.

- **状态 / Status** — not-started
- **证据 / Evidence** — —
- **卡点 / Blockers** — none
- **依赖 / Depends on** — T6.1

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
- **依赖 / Depends on** — T0.3
