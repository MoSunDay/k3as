# init-pro — Plan

> **Scope:** A from-scratch Rust implementation of a k3s-compatible
> Kubernetes distribution, bundled as **a single multicall binary**, with a
> first-class **built-in Router** on **openrusty (h2c, Q32)**. Forward
> delivery is a documented, supported Kubernetes protocol subset whose
> supported APIs remain upstream wire-compatible (Q59), not a claim of full
> conformance.

This directory is the **planning artifact** for the rewrite. No implementation
is advanced by these files; they define *what* to build, *how* to verify it,
and *in what order*. The plan itself is TODO **T0.5**.

---

## 1. Goal

Build **init-pro** — a single binary that, by changing its `argv[0]`, acts as
`init-pro server | agent | kubectl | ctr | containerd | …`, embedding etcd +
containerd + the CNI stack, exposing a **supported, upstream-wire-compatible
Kubernetes API subset (Q59)**, and shipping a **built-in HTTP Router** on the
openrusty h2c gateway (Q32). Ingress `networking.k8s.io/v1` objects compile to
Router configuration, and the Router remains a global platform configuration
variable.

**Current direction (Q59–Q63):** k3as is a multi-tenant Agent sandbox
platform. Its supported deployment is **one tenant per cluster**, with every
node dedicated to that tenant; a Kubernetes Namespace is not the tenant
security boundary. Phase 3 P0 adds T7.7 Agent action interception and Linux
sandboxing, extending done T7.1 without reopening it. The independent
inference track is P1 T4.15 host-direct execution, P2 T7.2 whole-GPU
allocation, and P3 T7.6 llama.cpp serving through a standard Deployment +
NodePort Service. Q60 host network/rootfs/device reuse is a performance
profile for ordinary inference only inside a tenant-dedicated cluster, not
the Agent action-containment profile; sandboxed Agents use Q63 instead.

### Non-goals (this plan cycle)
- Full/100% Kubernetes protocol conformance; forward scope is the documented
  supported subset (Q59).
- Production-grade performance tuning (correctness first).
- Windows support (Linux x86_64 first; structure to allow ports later).
- A new protocol or CLI vocabulary (supported k3s/k8s compatibility remains
  mandatory).

---

## 2. Locked decisions (see `decisions.md` for rationale)

| ID | Decision |
|----|----------|
| Q1 | **C — Hybrid + hard constraint: exactly one binary.** Multicall via `argv[0]` (mirrors k3s `bin/k3s`). etcd/containerd bundled by embed / FFI / subprocess. |
| Q2 | **Historical: Full k3s/k8s protocol conformance.** `kubectl`, `helm`, standard CRDs, native API groups — all wire-compatible. Nothing reinvented. **Narrowed by Q59 for forward delivery only:** supported APIs remain wire-compatible; no 100%/full-conformance claim. |
| Q3 | **AI Agent workloads = Phase 2, Layer 7.** Not in the critical path of Layers 0–6. |
| Q4 | **Built-in Router = first-class internal component, NOT an addon.** The original openresty-Rust/Lua DSL and Ingress-to-Lua design is historical; Q32 realizes the Router on openrusty (h2c). |
| Q5 | **Historical first vertical slice = built-in Router + Lua** (highest risk first, de-risked before everything else). The Lua wording records the original spike, not the current Q32 data plane. |
| Q6 | **Packaging topology:** subprocess bundling + per-file zstd embed (`build.rs` → `assets.rs`); offline `vendor/bin/` fallback. etcd FFI deferred to T2.1. |
| Q7 | **Licensing/SBOM/size:** per-build SPDX SBOM + `LICENSES/` + license allow-list gate; GPL `k3s-root` excluded in v1; soft size budget. |
| Q8 | **Config-file pre-scan:** ported `configfilearg` (`--config`/`-c` + env + default + `key+`); `.d/` dropins & http-config deferred. |
| Q9 | **Flag v1 posture:** max compatibility — accept-wired (Phase-1 subset) / accept-no-op-warn (rest) / fatal (conflicts). Matrix in `plan/00-foundation-flag-matrix.md`. |
| Q10 | **JSON-only wire format for v1** — API server, etcd storage, and watch all use `application/json`; protobuf deferred (see `decisions.md` Q10). Unblocks T1.1/T1.2/T2.2. |
| Q59 | **Forward delivery scope = supported Kubernetes subset.** Supersedes Q2 only for future scope; supported APIs stay upstream wire-compatible, Q10 JSON-only remains unchanged, and T0.6 stays done as the supported-subset golden gate. No 100%/full-conformance claim. |
| Q60 | **T4.15 host-direct performance profile.** Ordinary inference workloads inside a tenant-dedicated cluster may use host network/rootfs and clone3 necessary namespaces (`NEWNS`/`NEWPID`/`NEWUTS`/`NEWIPC`/`NEWCGROUP`, no `NEWNET`); literal `init-pro.io/rootfs: /` means no image pull/copy/pivot for `/`, with host `/dev` access and no containerd fallback (silent or explicit). This is not Agent action containment or a cross-tenant boundary; nodes must not be shared across tenants. T4.11–T4.14/Q48–Q55 remain done. |
| Q61 | **T7.2 direct whole-GPU discovery/allocation.** Builtin first for ordinary inference inside a tenant-dedicated cluster, using standard Kubernetes extended resources; no device-plugin or fractional-GPU support in the first slice and no containerd fallback (silent or explicit) for GPU requests. Fake tests provide routine, fake-scope evidence only; real-hardware execution may be opt-in for ordinary CI, but recorded real evidence for every claimed GPU vendor is mandatory to close whole T7.2. |
| Q62 | **T7.6 llama.cpp serving.** A standard Deployment + NodePort Service exposes an OpenAI-compatible API over pre-staged GGUF; no new CRD and no Agent reuse. Fake tests provide routine, fake-scope evidence only; real-hardware execution may be opt-in for ordinary CI, but closing whole T7.6 requires recorded real evidence with pinned real llama-server, local read-only GGUF, and a real GPU. |
| Q63 | **T7.7 Agent action interception + Linux sandbox.** A node/operator-owned default-deny policy gates every tool and subagent; existing nsrun/clone3/cgroup facilities enforce private non-host filesystem/network, least privilege, and resource/descendant containment. A per-attempt Unix-socket broker injects provider credentials upstream without exposing them to Agent environments. Sandboxed Agents never use Q60 host root/network/devices or fall back when sandboxing is unavailable. |

---

## 3. Milestones

| M# | Name | Exit criteria | Unlocks |
|----|------|---------------|---------|
| M0 | Foundation green | T0.1–T0.6 done; supported-subset golden gate runs in CI; `init-pro` multicall dispatches all aliases | all later layers |
| M1 | Historical Router spike (de-risk) | **Historical pre-Q32 criteria:** T5.1–T5.4 done; an Ingress compiles to Lua and serves traffic; resty::* subset works. Current Router: openrusty h2c (Q32). | confidence to build the platform |
| M2 | API + storage | T1.x + T2.x; `kubectl apply/get/watch` round-trips through embedded etcd | control plane |
| M3 | Control plane + node | T3.x + T4.x; a pod scheduled by init-pro runs in bundled containerd | end-to-end cluster |
| M4 | Router integrated | T5.5–T5.7; Router is a platform config var; ServiceLB + hot reload live | production-ish data plane |
| M5 | Addons + agent scheduling | T6.x + T7.1–T7.5; CoreDNS/local-storage auto-deploy; AI agent CRD schedulable | Phase 2 features |
| M6 | Multi-tenant Agent sandbox + host-native AI | P0 T7.7 Agent action interception and Linux containment; independent P1–P3 inference track T4.15 → T7.2 → T7.6. One tenant per dedicated-node cluster; Q60 is an ordinary-inference performance profile, never the Agent action-containment boundary | Phase 3 sandbox and host-AI delivery |

---

## 4. Historical verify-first spike (Q5)

The **single binary constraint (Q1)** and the **built-in Router (Q4)** were the
two highest-risk bets. Per Q5, the original plan proved them *first*, before
committing to the full platform build:

1. Produce a `init-pro` binary that dispatches on `argv[0]` (M0/T0.1).
2. Inside that same binary, run the Router data plane on Lua phase hooks
   (M1/T5.1–T5.4).
3. Feed it a Kubernetes `Ingress` object and assert it compiles to a Lua route
   that proxies traffic.

The Lua steps above are retained only as the historical M1 spike. Q32 later
replaced that implementation with the native Rust openrusty h2c data plane;
they do not describe the current Router. The original fallback was to
re-evaluate Q4 if M1 failed, before Layers 1–4 were built on top.

---

## 5. Reading order

1. **`index.md`** — the SSOT. All 54 TODOs, 7 fields each, status table, DAG.
2. **`decisions.md`** — Q1–Q63 ADR-style rationale (Q1–Q5 strategic; the rest are per-layer implementation decisions recorded as each layer landed, including Q56–Q58 for Layer 8 and Q59–Q63 for the forward sandbox and host-native AI scope).
3. **`template/TODO.md`** — the 7-field schema every TODO obeys.
4. **`plan/00-foundation.md` … `plan/08-delivery.md`** — per-layer
   detail (TODO IDs strictly mirror `index.md`; maintain both in lock-step).
5. **`plan/00-foundation-flag-matrix.md`** — the frozen k3s flag → init-pro v1
   behavior matrix (Q9 baseline for `init-pro server/agent --help`).
6. **`phase-3-host-ai.md`** (active) · **`phase-2-implementation.md`**
   (historical) · **`phase-1-implementation.md`** (archived at Sprint 9) —
   per-phase sprint-by-sprint execution plans; TODO IDs strictly mirror
   `index.md`.

---

## 6. Status legend

| Token | Meaning |
|-------|---------|
| `not-started` | No work begun. |
| `in-progress` | Actively being worked. |
| `blocked` | Cannot proceed; see `卡点` field. |
| `done` | Meets `验收手段`; evidence in `证据` field. |

At the initial planning baseline, all TODOs shipped as `not-started` **except
T0.5**, which this very file set completed (`in-progress` → `done` on merge).
