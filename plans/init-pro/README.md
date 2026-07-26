# init-pro — Plan

> **Scope:** A from-scratch Rust reimplementation of a k3s-compatible,
> fully-protocol-standard Kubernetes distribution, bundled as **a single
> multicall binary**, with a first-class **built-in Router** (an openresty
> Rust equivalent whose data plane is driven by Lua).

This directory is the **planning artifact** for the rewrite. No implementation
is advanced by these files; they define *what* to build, *how* to verify it,
and *in what order*. The plan itself is TODO **T0.5**.

---

## 1. Goal

Build **init-pro** — a single binary that, by changing its `argv[0]`, acts as
`init-pro server | agent | kubectl | ctr | containerd | …`, embedding etcd +
containerd + the CNI stack, exposing a **100 % k3s/k8s protocol-compatible**
API, and shipping a **built-in HTTP Router** (openresty Rust port) where
Ingress objects compile down to Lua routes and the Router is exposed as a
global platform configuration variable.

### Non-goals (this plan cycle)
- Production-grade performance tuning (correctness first).
- Windows support (Linux x86_64 first; structure to allow ports later).
- A new protocol or CLI vocabulary (k3s/k8s compatibility is mandatory).

---

## 2. Locked decisions (see `decisions.md` for rationale)

| ID | Decision |
|----|----------|
| Q1 | **C — Hybrid + hard constraint: exactly one binary.** Multicall via `argv[0]` (mirrors k3s `bin/k3s`). etcd/containerd bundled by embed / FFI / subprocess. |
| Q2 | **Full k3s/k8s protocol conformance.** `kubectl`, `helm`, standard CRDs, native API groups — all wire-compatible. Nothing reinvented. |
| Q3 | **AI Agent workloads = Phase 2, Layer 7.** Not in the critical path of Layers 0–6. |
| Q4 | **openresty Rust port = the built-in Router (first-class internal component, NOT an addon).** Lua is that Router's config/runtime DSL; Ingress compiles to Lua routes. |
| Q5 | **First vertical slice = built-in Router + Lua** (highest risk first, de-risked before everything else). |
| Q6 | **Packaging topology:** subprocess bundling + per-file zstd embed (`build.rs` → `assets.rs`); offline `vendor/bin/` fallback. etcd FFI deferred to T2.1. |
| Q7 | **Licensing/SBOM/size:** per-build SPDX SBOM + `LICENSES/` + license allow-list gate; GPL `k3s-root` excluded in v1; soft size budget. |
| Q8 | **Config-file pre-scan:** ported `configfilearg` (`--config`/`-c` + env + default + `key+`); `.d/` dropins & http-config deferred. |
| Q9 | **Flag v1 posture:** max compatibility — accept-wired (Phase-1 subset) / accept-no-op-warn (rest) / fatal (conflicts). Matrix in `plan/00-foundation-flag-matrix.md`. |
| Q10 | **JSON-only wire format for v1** — API server, etcd storage, and watch all use `application/json`; protobuf deferred (see `decisions.md` Q10). Unblocks T1.1/T1.2/T2.2. |

---

## 3. Milestones

| M# | Name | Exit criteria | Unlocks |
|----|------|---------------|---------|
| M0 | Foundation green | T0.1–T0.6 done; golden conformance subset runs in CI; `init-pro` multicall dispatches all aliases | all later layers |
| M1 | Router spike (de-risk) | T5.1–T5.4 done; an Ingress compiles to Lua and serves traffic; resty::* subset works | confidence to build the platform |
| M2 | API + storage | T1.x + T2.x; `kubectl apply/get/watch` round-trips through embedded etcd | control plane |
| M3 | Control plane + node | T3.x + T4.x; a pod scheduled by init-pro runs in bundled containerd | end-to-end cluster |
| M4 | Router integrated | T5.5–T5.7; Router is a platform config var; ServiceLB + hot reload live | production-ish data plane |
| M5 | Addons + agent scheduling | T6.x + T7.x; CoreDNS/local-storage auto-deploy; AI agent CRD schedulable | Phase 2 features |

---

## 4. Verify-first spike (Q5)

The **single binary constraint (Q1)** and the **built-in Router (Q4)** are the
two highest-risk bets. Per Q5 we prove them *first*, before committing to the
full platform build:

1. Produce a `init-pro` binary that dispatches on `argv[0]` (M0/T0.1).
2. Inside that same binary, run the Router data plane on Lua phase hooks
   (M1/T5.1–T5.4).
3. Feed it a Kubernetes `Ingress` object and assert it compiles to a Lua route
   that proxies traffic.

If M1 fails, Q4 is re-evaluated before Layers 1–4 are built on top.

---

## 5. Reading order

1. **`index.md`** — the SSOT. All 33 TODOs, 7 fields each, status table, DAG.
2. **`decisions.md`** — Q1–Q9 ADR-style rationale (Q1–Q5 strategic; Q6–Q9 T0.2/T0.4 implementation-level).
3. **`template/TODO.md`** — the 7-field schema every TODO obeys.
4. **`plan/00-foundation.md` … `plan/07-agent-scheduling.md`** — per-layer
   detail (TODO IDs strictly mirror `index.md`; maintain both in lock-step).
5. **`plan/00-foundation-flag-matrix.md`** — the frozen k3s flag → init-pro v1
   behavior matrix (Q9 baseline for `init-pro server/agent --help`).

---

## 6. Status legend

| Token | Meaning |
|-------|---------|
| `not-started` | No work begun. |
| `in-progress` | Actively being worked. |
| `blocked` | Cannot proceed; see `卡点` field. |
| `done` | Meets `验收手段`; evidence in `证据` field. |

All TODOs ship as `not-started` **except T0.5**, which this very file set
completes (`in-progress` → `done` on merge).
