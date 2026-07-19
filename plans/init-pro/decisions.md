# Decisions (ADR-style)

Locked during the planning clarification round. Each decision lists
**Context → Options considered → Decision → Consequences**.

---

## Q1 — Binary topology

**Context.** k3s ships one binary, `bin/k3s`, and dispatches on
`filepath.Base(os.Args[0])` (see `link_repos/k3s/cmd/k3s/main.go:166`).
Bundled peers — containerd, ctr, crictl, kubectl, cni plugins — are
`//go:embed`-staged out of the same binary
(`link_repos/k3s/pkg/data/data.go`, `pkg/deploy/stage.go`).

**Options.**
- A) Many binaries (Rust microservices, one per daemon).
- B) One binary, pure Rust (rewrite etcd + containerd in Rust).
- C) One binary, **hybrid bundling** — Rust core + embed/FFI/subprocess
  for etcd & containerd.

**Decision: C.** Exactly one binary, multicall via `argv[0]`. The
`externalCLIActions` set (k3s: `["crictl", "ctr", "kubectl"]`) becomes
init-pro aliases of the same `init-pro` executable.

**Consequences.**
- `+` Single artifact to ship/sign/air-gap; matches k3s ops model.
- `+` Symlink deployment (`ln -s init-pro kubectl`) "just works".
- `−` Embedding foreign binaries bloats the image; must dedupe via
  compression (k3s uses a tarball stage; init-pro will embed gz/typed assets).
- `−` Cannot rewrite etcd/containerd in Rust without exploding scope;
  accept FFI/subprocess bundling as the pragmatic path.
- → **T0.1** defines the multicall dispatch contract; **T0.2** defines the
  bundling pipeline.

---

## Q2 — Protocol compatibility

**Context.** A Kubernetes distribution that fails `kubectl apply` or a
standard CRD is not a Kubernetes distribution. Reinventing the API surface
doubles the work and breaks every downstream tool.

**Options.**
- A) Custom API/CLI to "fit Rust idioms".
- B) Compatibility at the resource-model level only.
- C) **Full k3s/k8s protocol conformance** — kubectl, helm, standard CRDs,
  native API groups, watch/etcd wire formats, kubeconfig/token auth.

**Decision: C.** Wire-compatible end to end. We consume the real
`kube-rs` client and `kubectl`/`helm` as our own acceptance clients.

**Consequences.**
- `+` Every existing tool works unmodified.
- `+` Lets us adopt upstream conformance tests as our golden gate (**T0.6**).
- `−` Serialization (etcd protobuf/json, StrategicMergePatch, protobuf API)
  must be byte-faithful — a non-trivial Rust effort.
- → **T0.6** pins a k3s conformance/e2e subset as the immutable golden set;
  every later TODO must keep it green.

---

## Q3 — AI Agent workloads

**Context.** The motivating workload for init-pro is AI agent orchestration, but
it is not on the critical path of "be a k3s-compatible distro".

**Options.**
- A) Fold agent semantics into the core scheduler now.
- B) **Phase 2, Layer 7** — a CRD + scheduler extender layered on the
  finished platform.

**Decision: B.** AI Agent scheduling is **Layer 7**, gated behind
M5 (after Layers 0–6 are done). The platform is a generic k3s-compatible
cluster first.

**Consequences.**
- `+` Keeps Layers 0–6 focused on correctness/conformance.
- `+` Agent features land as standard CRDs + scheduler plugins (Q2-compatible).
- `−` Must design the scheduler extender seam now (T3.2) so Layer 7 fits.

---

## Q4 — The built-in Router (openresty Rust port)

**Context.** openresty = nginx + LuaJIT + the `resty::*` library ecosystem.
Its phase model (`init_by_lua`, `rewrite_by_lua`, `access_by_lua`,
`content_by_lua`, `header_filter_by_lua`, `body_filter_by_lua`,
`log_by_lua`, `balancer_by_lua`, `init_worker_by_lua`) is what makes it
programmable. We want that programmability, but in Rust, and as a
**platform primitive**, not a sidecar addon.

**Options.**
- A) Ship nginx/openresty as a bundled addon binary.
- B) Write a Rust HTTP router, no Lua.
- C) **Rust router whose data plane is driven by Lua** (via `mlua`), exposed
  as a first-class internal component; Ingress objects compile to Lua routes.

**Decision: C.** The Router is an internal first-class component (not an
addon). Lua is the Router's config + runtime variable language. Ingress →
Lua route compilation is the primary integration path.

**Consequences.**
- `+` Programmable data plane; no fork-of-nginx maintenance burden.
- `+` Router state can be exposed as a **platform configuration variable**
  (T5.7) — node/schedule/network-policy code reads Router variables.
- `−` Must rebuild the `resty::*` standard library (T5.3) and the
  phase-hook pipeline (T5.2) ourselves.
- `−` coroutine↔async bridging (T5.1) is the technical crux.
- → **Layer 5 repositioned**: not "Ingress addon" but "built-in Router".

---

## Q5 — First vertical slice

**Context.** Two bets dominate risk: the single-binary constraint (Q1) and
the Lua-driven Router (Q4). We have finite attention; de-risk the bigger
unknown first.

**Options.**
- A) Build bottom-up (Layer 0 → 7 in order).
- B) **De-risk Q1+Q4 first**, then build the platform.

**Decision: B.** The first deliverable after M0 is the **Router + Lua slice
(M1)**: same binary, Lua phase hooks, Ingress→Lua route compilation,
`resty::*` subset. Only after that passes do we commit to Layers 1–4.

**Consequences.**
- `+` If the Router bet is wrong, we learn before investing in storage/API.
- `+` Produces a demo-able artifact early.
- `−` Inverts the natural dependency order; M1 is allowed to stub Layers 1–4
  just enough to feed an Ingress to the Router.
- → Milestone table (README §3) reflects this ordering.
