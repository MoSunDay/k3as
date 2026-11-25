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

---

## Q6 — Packaging topology (T0.2)

**Context.** k3s ships all foreign binaries as a single zstd-compressed
tarball embedded via `//go:embed`, content-addressed by its own SHA-256
(`link_repos/k3s/scripts/package-cli:57-74`; zstd `-16 --long=25`; runtime
`extract()` in `cmd/k3s/main.go:259-375`). The embedded tree is `bin/`
(k3s multicall + containerd/runc/CNI multicall) and `bin/aux/` (host
utilities). **Critical audit finding:** etcd is *not* a bundled binary in
k3s — it is linked as an in-process Go library
(`pkg/executor/embed/etcd/etcd.go`, `go.etcd.io/etcd/server/v3/embed`).
T0.2 therefore bundles only subprocess binaries (containerd, runc, CNI
multicall); etcd embed/FFI is T2.1's concern.

**Options.**
- A) Mirror k3s exactly: one `tar.zst` blob embedded whole; runtime
  untar + verify.
- B) **Per-file zstd embed** — `build.rs` fetches pinned artifacts,
  compresses each file independently, and generates an `assets.rs` (Rust
  `go:embed` equivalent via `include_bytes!`); runtime `stage()` writes
  per-file and verifies a ported `.sha256sums` + `.links` manifest.
- C) `rust-embed` crate at compile time over a `vendor/bin/` directory.

**Decision: B.** Subprocess bundling only (no etcd FFI in T0.2 — deferred
to T2.1). `build.rs` downloads pinned versions (SHA-256-gated) into a
gitignored `vendor/bin/`, emits `.sha256sums` + `.links` manifests (ported
from `pkg/dataverify/dataverify.go`), compresses each file with zstd, and
generates `assets.rs`. An `INIT_PRO_OFFLINE=1` env var forbids network and
requires a pre-populated `vendor/bin/` (the offline fallback). Runtime
`stage()` mirrors k3s `extract()`: flock → write-to-tmp → `dataverify` →
atomic rename → set child `PATH` (CNI dir first, then host PATH vs
`bin/aux` toggled by `--prefer-bundled-bin`, `cmd/k3s/main.go:218-237`).

**Consequences.**
- `+` Per-file embed simplifies `stage()` and gives per-artifact
  integrity; matches the `init-pro stage --dry-run` contract (list each
  artifact + SHA-256).
- `+` Offline fallback (`vendor/bin/` + `INIT_PRO_OFFLINE=1`) keeps
  CI/air-gap builds deterministic (cf. risk R-vendor-network).
- `−` Per-file zstd loses k3s's cross-file long-distance matching
  (`--long=25` exploits that many similar statically-linked ELFs share
  code blobs); expect a larger compressed payload than k3s's single
  tarball. Mitigation: target zstd level 19 and a soft size budget (Q7);
  revisit a single-tarball mode only if the budget busts.
- `−` `build.rs` doing network I/O complicates reproducible builds;
  mitigated by the offline fallback and `--locked`.
- → **T0.2** owns this pipeline; **T2.1** owns etcd embed/FFI/subprocess
  bundling separately.

---

## Q7 — Licensing, SBOM, and size budget (T0.2)

**Context.** k3s ships only its own Apache-2.0 `LICENSE` and performs **no**
SBOM or third-party notice aggregation (audit: no `LICENSES/`, no `NOTICE`,
zero `spdx|syft|sbom|cyclonedx` matches in tree). The v1 bundled set —
containerd, runc, CNI multicall — is **Apache-2.0** upstream (confirmed).
**Audit warning:** k3s's `k3s-root` host utilities (iptables, socat,
ebtables, ethtool, busybox multicall) are largely **GPL-2.0**; bundling
those triggers GPL notice/redistribution obligations that k3s handles
implicitly via its separate air-gap tarball. k3s caps its final multicall
binary at **80 MiB** (`scripts/binary_size_check.sh:14-17`), but that
excludes the unpacked foreign binaries (they live under
`<data-dir>/data/<HASH>/bin/` only after first run).

**Options.**
- A) Match k3s: ship only init-pro's own LICENSE; ignore component notices.
- B) **Per-build auto-generated SPDX SBOM + `LICENSES/` notice tree +
  license allow-list gate.**
- C) Hand-maintained `THIRD_PARTY` file.

**Decision: B.** Each build (a) collects every vendored component's
upstream `LICENSE`/`NOTICE` into a `LICENSES/` tree and a generated
SPDX-2.3 SBOM, and (b) enforces a **license allow-list gate** — any
artifact whose license is not on the cleared list (Apache-2.0, BSD-2/3,
MIT, ISC) **fails the build**. The v1 bundled set is Apache-2.0 (cleared).
**GPL-2.0 host utilities (k3s-root) are explicitly excluded from the v1
bundle** and deferred; if later needed they require additional GPL notice
handling under this same gate. **Size budget:** soft — track total
compressed-blob size per Q6 (zstd-19 target); warn past 80 MiB parity,
fail past a hard cap to be set when the real v1 bundle is first measured.

**Consequences.**
- `+` Compliance surface is explicit and automated; no silent GPL
  inheritance.
- `+` Reproducibility: SBOM + `--locked` + pinned SHA-256 = auditable
  provenance.
- `−` Adds a build step (license fetch + SBOM generation); each new
  vendored artifact must clear the gate before bundling.
- `−` Excluding k3s-root means init-pro v1 does **not** ship bundled
  iptables/socat; it relies on the host for those (documented; T4.3
  networking may revisit).
- → **T0.2** acceptance includes "SBOM generated + license gate green";
  `init-pro stage --dry-run` prints the SBOM reference.

---

## Q8 — Config-file pre-scan (T0.4)

**Context.** k3s pre-scans `--config`/`-c`/`K3S_CONFIG_FILE` **before**
the CLI parser runs (`pkg/configfilearg/`, esp. `parser.go`,
`defaultparser.go`). Resolution order: env `K3S_CONFIG_FILE` → CLI
`--config/-c` → default `/etc/rancher/k3s/config.yaml`. Config-file values
are injected right after the command word so **CLI flags override**; slice
flags **append**. A `key+` suffix means "append to slice"
(`parser.go:279-294`). Per-command invalid-flag stripping applies only to
commands in a `ValidFlags` map (`server`, `etcd-snapshot` — notably **not**
`agent`). `.d/` dropin directories merge after the base file.
`MustFindString` short-circuits on `--help/-h/--version/-v`.

**Options.**
- A) Port the full `configfilearg` machinery (files + env + dropins + http
  configs + `key+` + per-command stripping) in v1.
- B) **v1 = file + env + `--config/-c` + `key+` + per-command invalid-flag
  stripping; defer `.d/` dropins and http-config sources.**
- C) No config-file pre-scan in v1; flags only.

**Decision: B.** Port `configfilearg` semantics to a **pre-clap layer** in
`cli`: resolution order env `INIT_PRO_CONFIG_FILE` →
`--config`/`-c` → default `<data-dir>/config.yaml`; injected after the
command word so CLI wins; slice flags append; `key+` = append-to-slice;
per-command invalid-flag stripping (ported `stripInvalidFlags`, applied to
`server` — agent left pass-through as in k3s); `--help/-h/--version/-v`
short-circuit. **`.d/` dropins and http-config sources are explicitly
deferred** to a later T0.4 point-release, with rationale: they add
filesystem-walking and HTTP fetch to the pre-parse path and are not on the
Phase-1 critical path.

**Consequences.**
- `+` Existing k3s `config.yaml` files keep working for the supported
  subset; operator scripts don't break (Q2).
- `+` Pre-clap layer keeps clap-derive clean (Q9 matrix is the clap
  surface; config is a separate concern).
- `−` Dropin/http users must consolidate to a single file in v1 —
  documented, single-line deferral.
- `−` Must mirror k3s's "agent does not strip invalid flags" quirk exactly
  or operators hit surprising errors.
- → **T0.4** owns the pre-scan + clap surface; flag categorization lives
  in Q9 / `plan/00-foundation-flag-matrix.md`.

---

## Q9 — Flag v1 posture & matrix (T0.4)

**Context.** k3s `server` exposes ~88 flag entries and `agent` ~45 (audit:
`pkg/cli/cmds/server.go:190`, `agent.go:269`), many shared via reused flag
vars. Q2 mandates wire compatibility; scripts and operators will pass the
full k3s flag vocabulary. A reimplementation that fatals on every
unimplemented flag breaks the compatibility promise; one that silently
ignores them hides configuration that does nothing.

**Options.**
- A) **Max compatibility: accept everything k3s accepts; Phase-1 subset is
  wired, the rest no-op-with-warning, only contradictions of Phase-1
  behavior are fatal.**
- B) Phase-1 subset only; fatal on all others.
- C) Accept everything silently.

**Decision: A.** Every k3s `server`/`agent` flag is categorized (see
`plan/00-foundation-flag-matrix.md`, frozen as the
`init-pro server --help` / `init-pro agent --help` diff baseline) into
exactly one of:
- **accept-wired** — Phase-1 subset, parsed and honored: `--data-dir/-d`,
  `--debug`, `--config/-c`, `--disable` (+ `DisableItems` validation:
  `coredns, servicelb, traefik, local-storage, metrics-server, runtimes`),
  `--disable-{etcd,apiserver,agent,controller-manager,scheduler,
  cloud-controller,kube-proxy,network-policy,helm-controller}`,
  `--datastore-endpoint`, `--prefer-bundled-bin`, `--token/-t`,
  `--server/-s`, `--cluster-init`.
- **accept-no-op-warn** — accepted, parsed, then logged once at WARN:
  *"flag `<f>` accepted but not yet implemented; no-op"* (deduped per
  process). Covers all remaining flags so operator scripts keep working.
- **fatal** — only when a flag value contradicts Phase-1 behavior or trips
  a k3s conflict rule (ported verbatim): e.g. `--disable-apiserver` ✗
  `--datastore-endpoint`, `--disable-etcd` ✗ `--datastore-endpoint`,
  `--cluster-reset-restore-path` without `--cluster-reset`, `--disable-etcd`
  without `--server` (audit: `pkg/cli/server/server.go:245-265`).

**Consequences.**
- `+` Maximum compatibility (Q2): k3s scripts run unmodified against
  init-pro for the wired subset and degrade gracefully elsewhere.
- `+` Operators get an explicit signal for no-op flags (not silent
  misconfiguration).
- `−` The matrix is large (~130 flags); mitigate with one-line-per-category
  entries and an automated parity test (`scripts/cli-flag-parity-test.sh`).
- `−` No-op warnings can be noisy; mitigate by deduping per process and
  honoring log level / `--quiet`.
- → **T0.4** implements clap-derive server/agent flag groups + the pre-scan
  (Q8) + `DisableItems` validation + conflict rules; the matrix is the
  frozen baseline; `scripts/cli-flag-parity-test.sh` enforces it.

---

## Q10 — Serialization wire format for v1 (JSON-only vs protobuf)

**Context.** T1.1 (resource model) must pick a serialization wire format.
Upstream Kubernetes uses a **dual codec**: protobuf for native core types
(the `application/vnd.kubernetes.protobuf` content type, driven by generated
`*.pb.go` + `runtime.ProtobufMarshaller`), and JSON for everything else
(CRDs, status, watch fallback). Faithful protobuf parity requires either
generating `.proto`-backed Rust bindings for every native type or vendoring
Go's `k8s.io/apimachinery` runtime — a large, fragile surface that would
block the critical path (T1.2/T2.2) behind an unbounded protobuf-fidelity
effort. JSON is lossless, ubiquitous, and `kubectl`/`kube-rs`/`helm` all
negotiate it transparently via the discovery `accept` header.

**Options.**
- A) **Full protobuf parity day-one** — generate Rust protobuf codecs for all
  core types before T1.2 ships.
- B) **JSON-only for v1; protobuf deferred** to a later decision/TODO.
- C) Mixed: JSON on the wire but protobuf inside etcd storage (mirrors
  upstream's `runtime.StorageSerializer` cost model).

**Decision: B.** init-pro v1 speaks **JSON only** on every path (API server
wire format, etcd storage encoding, watch streams). protobuf is explicitly
deferred: native etcd blobs will be JSON-encoded, and `Content-Type:
application/json` is the negotiated codec. `kubectl`/`kube-rs` detect
JSON-only servers via discovery and fall back automatically — no client-side
breakage (verified against the `Accept` negotiation in
`k8s.io/apimachinery/pkg/runtime/serializer`). The protobuf deferral is
recorded as a future decision gated behind a measured need (large-list
watch throughput on >10k-object clusters).

**Consequences.**
- `+` Unblocks T1.1/T1.2/T2.2 immediately; no protobuf codegen pipeline to
  build and keep in lock-step with upstream `k8s-openapi` releases.
- `+` Byte-faithful JSON round-trip (S4) becomes the *whole* fidelity bar —
  testable with `serde_json` against canonical fixtures (part of T0.6).
- `+` Storage debugging is human-readable (`etcdctl get` shows JSON).
- `−` Watch/list throughput on very large clusters is higher-bandwidth than
  protobuf; acceptable for v1 scale targets. Mitigation: watch
  `gzip`/chunked-transfer (T1.2) and a future protobuf decision if measured.
- `−` `kubectl explain` needs OpenAPI v2/v3 schema discovery (T1.1 ships the
  discovery skeleton in S7; full OpenAPI generation is a separate TODO).
- → **T1.1** ships JSON-only resource model + schema registry + StrategicMergePatch;
  **T1.2** serves JSON codecs end to end; a future `Q? — protobuf storage`
  decision may reopen this once T0.6 watch benchmarks exist.

---

## Q11 — HTTP framework & TLS posture for the API server (T1.2a)

**Context.** T1.2 serves the Kubernetes discovery surface over HTTP. We need
an async HTTP server framework shared across the codebase: the **apiserver**
(T1.2) and the **Router data plane** (T5.2, which the plan explicitly calls
"hyper body streaming") both need a hyper/tokio-based server. Choosing now
de-risks both the discovery slice and the critical-path Router. Separately,
kubectl refuses plain HTTP, so the TLS question is immediate for any
real-client interop.

**Options.**
- A) `actix-web` — feature-rich, but its own actor runtime diverges from the
  tokio/hyper substrate the Router (T5.2) and etcd client (T2.x) rely on.
- B) Raw `hyper` — maximal control, but we'd hand-roll routing/middleware that
  both the apiserver and the Router need.
- C) **`axum`** — built on hyper/tokio/tower, ergonomic routing + extractors,
  composable middleware, the de-facto Rust standard.

**Decision: C.** HTTP framework is **axum** (on hyper/tokio/tower). It is the
single server stack for the apiserver (T1.2) and the Router data plane (T5.2),
so the choice de-risks both paths at once. `Content-Type: application/json` is
the sole negotiated codec (Q10); axum's `Json` extractor/response sets it.

**TLS posture: plain HTTP for T1.2a; TLS deferred to T1.3.** T1.2a binds the
loopback and serves discovery over **plain HTTP**. Acceptance for this slice is
`curl` byte-equivalence (see `scripts/apiserver-discovery-parity-test.sh` and
`apiserver/tests/discovery_http.rs`), **not** real kubectl interop —
kubectl refuses plain HTTP, and certificate management is the natural neighbor
of the T1.3 identity/auth work. Real kubectl interop (TLS + persistence + watch)
is the deferred acceptance gate of **T1.2b**.

**修订注记 (Q47 定稿, S49 2026-11-21):** h2c 明文仅为默认姿态，TLS 为
opt-in（`--tls-san` + `--token-file`，Q47）；控制面 (apiserver + kubectl)
采用单一 rustls TLS 栈。golden h2c 基线不动；kubectl 真实 interop 验收门
随 T1.3a 兑现（S50 开工）。

**Consequences.**
- `+` One HTTP stack for two paths; the Router (T5.2) reuses axum's hyper body
  streaming instead of pulling in a second framework.
- `+` Discovery handlers are thin transport wrappers over the T1.1 builders —
  byte fidelity is already proven, so T1.2a is a low-risk slice.
- `−` kubectl cannot talk to T1.2a directly (no TLS). Mitigated: `curl`
  acceptance + the byte-fidelity integration test prove the wire shape; kubectl
  interop lands with T1.2b + T1.3.
- `−` Adding `axum`/`tower` grows the dependency graph. Mitigated: axum reuses
  transitive deps (hyper/tokio) already pulled in by the Router path.
- → **T1.2a** ships axum-backed discovery on plain HTTP; **T1.2b** adds the
  store trait + CRUD/watch (needs T2.2); **T1.3** adds TLS (rustls) + auth so
  kubectl interop becomes the acceptance gate.

---

## Q12 — Router VM model & the coroutine↔async bridge (T5.1)

**Context.** Q4 makes the built-in Router an openresty Rust port whose data
plane is driven by **Lua**. The single highest-risk unknown of the whole
project (per Q5's de-risk-first stance) is: **can a Lua coroutine yield at a
Rust `await` point on the Tokio runtime, letting another coroutine run
concurrently without blocking the worker thread?** If not, Q4 is infeasible and
must be re-evaluated before the platform is built on top of it. `mlua` 0.12
exposes `LuaJIT` (Lua 5.1 semantics, matching openresty's LuaJIT 2.1) with an
`async` feature and `create_async_function`/`call_async` APIs that park a Lua
coroutine while a Rust future is polled and resume it on completion.

**Options.**
- A) **Per-request VM on a multi-thread executor** (mlua `send` feature): each
  request gets its own `Send` Lua VM on a tokio thread pool. Parallelism comes
  from threads, like a classic HTTP server. But this diverges hard from the
  openresty model (one VM per worker, coroutine-level concurrency), forfeits
  shared VM state/caches, and pays full VM init cost per request.
- B) **Worker-wide VM + per-coroutine Lua threads on a single-thread async
  runtime** (`tokio::task::LocalSet`): one LuaJIT VM per worker; each request
  is a Lua coroutine driven as a Rust future via `call_async`; coroutines
  interleave cooperatively at `await` points — a faithful reproduction of
  openresty's per-worker coroutine scheduler.
- C) **Abandon Lua** — write the Router in pure Rust (no Ingress→Lua, no
  `resty::*` ecosystem). This discards Q4 entirely.

**Decision: B.** The Router VM is a **worker-wide LuaJIT VM** (built from source
via `mlua`'s `vendored` + `luajit` features, offline-friendly) carrying
**per-coroutine Lua threads**, driven on a **`tokio::task::LocalSet`**
(single-thread async runtime). The coroutine↔async bridge is `mlua`'s
`create_async_function` (registers a Rust async fn callable from Lua) +
`Function::call_async` (drives a Lua function as a coroutine, parking it at
each inner `await` and resuming on completion). Concurrency is **cooperative
yielding at `await` points**, exactly like openresty — there is one VM per
thread, so the `Lua: !Send` constraint is a non-issue (no cross-thread VM
sharing). `luajit52` is deliberately **off** (openresty = LuaJIT 2.1 = Lua 5.1,
not 5.2 extensions).

**Consequences.**
- `+` Faithful to openresty's worker model — ported `resty::*` libraries and
  phase hooks (T5.2/T5.3) land in familiar territory, and a worker-wide VM
  amortises compilation/state across requests.
- `+` **The bridge is proven real.** The T5.1 kill-criterion
  (`router/tests/concurrency.rs`) shows coroutine B starting and
  finishing **inside** coroutine A's `ngx.sleep(50ms)` window (order
  `A_start < B_start < B_end < A_end`), total wall ≈ max ≈ 50ms (not the serial
  sum); 10 coroutines × 20ms complete in ~21ms. `ngx.sleep(10ms)` round-trip is
  ~11ms. Q4 is de-risked; no Q4 re-evaluation needed.
- `+` One VM per thread sidesteps `!Send` without the per-request VM cost; the
  shared axum/tokio substrate (Q11) hosts both the data plane and the apiserver.
- `−` Parallelism is per-worker (one thread), not per-coroutine — a single
  worker is bounded by one core for Lua work. Mitigated: the Router runs
  multiple workers (one per core), exactly like openresty's `worker_processes`;
  CPU-light proxying streams bodies via hyper (T5.2) off the Lua path.
- `−` LuaJIT is built from source (`luajit-src`) on first compile (~25s); needs
  `gcc`/`make`. Mitigated: `vendored` makes it offline-reproducible and is
  consistent with the Q7 air-gap posture.
- → **T5.1** ships the spike (VM + `ngx.sleep` + concurrency/latency proof) as
  an independent crate, **not** wired into `init-pro server`. **T5.2** adds the
  HTTP phase pipeline + cosocket; **T5.3** the `resty::*` stdlib; **T5.4**
  Ingress→Lua route compilation (M1).

## Q13 — Per-request coroutine-local binding & the data-plane server shape (T5.2)

**Context.** Q12 fixed the VM model (one worker-wide LuaJIT VM, per-coroutine
threads on a `LocalSet`). T5.2's content phase needs **each in-flight request's
Lua coroutine to reach its own `RequestContext`** — the live `ngx.req`/
`ngx.header`/`ngx.status`/cosocket handles — without a global lock and without
leaking one request's state into another's coroutine. openresty does this
implicitly via per-request VM globals; our single shared VM must do it
explicitly. Two sub-problems surfaced in the same spike: (1) how to key a
context to the *right* coroutine, and (2) how to host the HTTP data-plane
server that drives these coroutines, given the VM is `!Send`.

**Options.**
- A) **`Lua::app_data` + `current_thread().to_pointer()` key (per-coroutine).**
  Store a `ContextStore: RefCell<HashMap<*const u8, Rc<RequestContext>>>` in the
  VM's `app_data`. At request start, `Lua::create_thread(content_fn)` makes a
  *real* coroutine thread, bind its `current_thread().to_pointer()` -> context,
  then drive it with `Thread::into_async`. Each `ngx.*` API looks up the running
  coroutine's pointer to find its context.
- B) **`call_async` implicit coroutine.** Drive `content_fn` with
  `Function::call_async`. Simpler — but mlua runs the function on the **root
  thread**, so `current_thread().to_pointer()` is identical for every request
  (collapses to one key). A spike proved it: distinct coroutines all report the
  *same* thread pointer, so any app_data key collides across concurrent
  requests. Unusable.
- C) **Per-request VM** (a fresh `Lua` per request). Removes the binding problem
  entirely, but contradicts Q12 (worker-wide VM amortising compile/state) and
  pays full VM init per request.

**Decision: A.** Per-request binding is **coroutine-local**: an explicit
`Lua::create_thread` + `Thread::into_async` drives a genuine per-request
coroutine, and `Lua::current_thread().to_pointer()` (the coroutine's stable
identity, distinct under interleaving — proven by the spike) keys a
`ContextStore` held in `app_data`. `context::bind`/`unbind`/`current` are the
only accessors; `current()` returns `Rc<RequestContext>` (cheap clone, no
locking). `RequestContext` carries the method/URI/headers, a status slot, an
ordered header bag, and the cosocket handle table. This keeps one shared VM
(Q12) while giving every request its own isolated `ngx.*` view.

**Related finding — the data-plane server is raw TCP, not axum.** Resolving the
binding exposed a second constraint: **axum's `Send` handler bound is
incompatible with the `!Send` Lua VM.** A handler that drives a Lua coroutine
cannot be `Send`, so it cannot be an axum `Router` route (Q11/Q12 assumed the
shared axum substrate would host the data plane — that part is **superseded for
the data plane**; the apiserver keeps axum). The Router data-plane server is a
small raw-TCP HTTP/1.1 loop (`serve.rs`): read request head, bind context,
drive the content-phase coroutine on the `LocalSet`, then write the assembled
response. Parallelism stays per-worker (one `LocalSet`), consistent with Q12.

**Consequences.**
- `+` Coroutine-local binding is correct under concurrency: the spike showed N
  interleaved requests each observe *their own* method/URI/status/headers, with
  no cross-talk. `tests/content_phase.rs` exercises this over real TCP
  (`real_client_concurrent_requests_stay_distinct_over_tcp`).
- `+` Real HTTP interop: `ngx.req`/`ngx.header`/`ngx.status`/`ngx.say`/
  `ngx.print`/`ngx.exit` are observed by a real TCP client
  (`real_client_observes_content_phase_over_tcp`).
- `+` Cosocket (`ngx.socket.tcp`: `connect`/`send`/`receive`/`settimeout`/
  `close`) streams to a real echo server; `send` takes Lua strings (binary-safe
  via `LuaString`), `receive` returns a Lua string (fixed-size and line modes).
- `−` Raw TCP means we re-implement a minimal HTTP/1.1 head parser + response
  writer rather than reusing hyper/axum for the data plane. Mitigated: the loop
  is ~210 lines and only handles what the content phase emits; the apiserver
  (T1.2) still benefits from axum.
- `−` `current_thread().to_pointer()` is an opaque identity, not a stable
  hashable value across VM resets; we never persist it. Mitigated: contexts are
  bound/unbound within the request future's scope, so pointers never outlive
  their request.
- → **T5.1 → done** (the spike is closed; nothing further is owed by T5.1).
  **T5.2 Scope A** (content phase + `ngx.*` + cosocket, verified by real HTTP
  clients) is complete. Remaining T5.2 scope — the full phase chain
  (`rewrite`/`access`/`header_filter`/`body_filter`/`log`/`balancer`),
  `ngx.var`/`ngx.shared.DICT`/`ngx.exec`/`ngx.redirect` — is T5.2 Scope B+.

---

## Q14 — Per-phase coroutines, body buffering & the filter semantics (T5.2 Scope B)

**Date.** 2026-07-26 (Sprint 6).

**Context.** Q13 established per-request coroutine-local binding: each
in-flight request's Lua coroutine gets its own `RequestContext`, looked up in a
`ContextStore` (in VM `app_data`) by `Lua::current_thread().to_pointer()`.
Scope A (Sprint 5) shipped only the `content` phase. Scope B must add the full
openresty phase chain — `init_worker`→`rewrite`→`access`→`content`→
`header_filter`→`body_filter`→`log` — plus request-body reading and the rest of
the `ngx.*` surface (`ngx.var`, `ngx.arg`, `ngx.exec`, `ngx.redirect`, time
helpers). Two design questions dominated: how to chain the phases, and how to
feed the request body to Lua.

**Decision A — one explicit Lua `Thread` per phase, sharing one context.**
Rather than one long-lived "driver" coroutine that yields between phases, each
phase runs as its **own** `Lua::create_thread` + `Thread::into_async` coroutine.
All phases of a single request share the *same* `Rc<RefCell<RequestContext>>`
(this is what the Q13 pointer lookup returns). Why per-phase threads:
- The short-circuit signals — `ngx.exit(STATUS)`, `ngx.exec(uri)`,
  `ngx.redirect(url)` — unwind the running coroutine via a sentinel
  `mlua::Error` (`PhaseAborted`). Cleanly propagating that **per phase** (run
  the thread, inspect the outcome, decide whether to continue) is far simpler
  than catching-and-continuing inside a single driver coroutine, where the Lua
  stack would carry the abort across the phase boundary.
- Phase isolation: a hard Lua error / panic in one phase cannot corrupt the
  next phase's Lua stack (each has its own thread).
- Distinct threads → distinct `current_thread().to_pointer()` values, yet the
  Q13 lookup still resolves to the one shared context (`RequestContext` holds
  the per-request `Thread`s in a small map, or they're re-entrant on the same
  binding). Binding/unbind per phase is cheap (one `Rc` clone).

**Phase ordering & short-circuit rules.** `rewrite`→`access`→`content` are
*generative*: an `ngx.exit` / `ngx.redirect` / `ngx.exec` in any of them stops
further generative phases (a `rewrite` exit skips `content`). The *filter*
phases (`header_filter`, `body_filter`) **always run**, even after a content
short-circuit — that is how a `header_filter_by_lua` still mutates a 302 from
`ngx.redirect`. `log` runs last (fire-and-forget; the response is already
captured). `init_worker_by_lua` runs once at worker boot, before any request.

**`ngx.exec` re-dispatch.** An internal redirect re-runs the generative phases
for the rewritten URI (openresty semantics). It is loop-guarded
(`MAX_EXEC_REDIRECTS = 10`) so a Lua bug cannot infinite-loop the worker.

**Decision B — buffered request body, with chunked decoding and a size cap.**
The data plane (`serve.rs`) now buffers the request body before the phase runs,
honouring `Content-Length` **or** `Transfer-Encoding: chunked` (decoded),
capped at `MAX_BODY_BYTES = 1 MiB` (overflow → `413 Request Entity Too Large`).
The body lives in `RequestContext::req_body` (`Option<Vec<u8>>`); `ngx.req.
read_body()` materialises it, `get_body_data()` returns it, `get_post_args()`/
`get_query_args()` parse it as `application/x-www-form-urlencoded`. This matches
openresty's buffered mode for small bodies, which covers the Ingress use case.

**Decision C — `body_filter` is a buffered whole-body transform.** `ngx.arg[1]`
is primed with the fully assembled body, the user filter mutates it, and the
result is committed back to the response. This is **not** true chunked
streaming (openresty's `ngx.arg[1]` is a stream chunk). It is flagged as a
known limitation; true streaming `body_filter` is deferred to **T5.6** (the
ServiceLB data-plane work), where the body plumbing must be reworked for
streaming proxying anyway.

**Consequences.**
- `+` The phase chain is composable (`Pipeline::build()` builder; `Pipeline::
  new(src)` kept as the content-only convenience) and generic: a future
  `balancer_by_lua` (T5.4) or `ssl_certificate_by_lua` can register with the
  same mechanism.
- `+` Short-circuit semantics are openresty-faithful: filters run after a
  content exit; `ngx.exec` re-dispatches; `ngx.redirect` emits the HTTP
  redirect. All verified by `tests/phase_chain.rs` (15 tests, including the
  §6 gate `real_client_observes_header_filter_mutation`).
- `+` Request bodies round-trip: POST Content-Length **and** chunked bodies are
  readable from Lua; the 1 MiB cap protects the worker from unbounded buffering.
- `−` Buffered body + buffered `body_filter` means the whole response body is
  materialised in memory per request. Acceptable for v1 Ingress (headers/health
  probes/small payloads); streaming is T5.6. **Documented limitation.**
- `−` The data plane re-implements chunked-TE decoding (~40 lines) rather than
  reusing hyper's body framing; consistent with the Q13 raw-TCP decision.
- → **T5.2 → done.** Scope A (content + cosocket) + Scope B (full phase chain,
  body, `ngx.var`/`ngx.arg`/`ngx.exec`/`ngx.redirect`/time) are complete and
  verified by real HTTP clients. Unblocks **T5.4** (M1 Ingress spike) once
  **T5.3** (`resty::*`, which owns `ngx.shared.DICT`) lands.

## Q15 — Auto-created shared dicts & the resty::* storage model (T5.3 Scope A)

**Date.** 2026-07-26 (Sprint 7).

**Context.** T5.3 must give Lua the `resty::*` standard library users expect
from openresty, and — critically — the `ngx.shared.DICT` surface, which is the
worker-process shared-state layer a router needs (rate-limit counters, A/B
decision tables, dedup caches). Two questions dominated: (1) how do shared
dicts *come into existence* when there is no `lua_shared_dict` directive yet
(Ingress config compilation is **T5.4**, and no nginx-style config exists at
all in k3as), and (2) where does the state live given the Q12 single-threaded
`!Send` VM model.

**Decision A — shared dicts are auto-created on first access.** In openresty,
`ngx.shared.foo` is an error *unless* the operator pre-declared it with
`lua_shared_dict foo 1m;`. k3as deliberately **deviates**: accessing an
unknown `ngx.shared.<name>` lazily materialises a new dict (default capacity)
rather than raising. The mechanism is an `__index` metamethod on the
`ngx.shared` proxy: a miss falls through to `SharedDictRegistry::get_or_create`,
which inserts a fresh `Rc<RefCell<SharedDictStore>>` into the registry and
returns a `SharedDictHandle` userdata bound to it. This avoids a chicken-and-egg
dependency on T5.4 config (Lua can `ngx.shared.foo:set(...)` today with zero
config plumbing) and matches the "compile to Lua routing tables" ethos — the
Ingress compiler will simply *use* dicts that already self-initialise.

**Decision B — per-worker `RefCell` state, no `DashMap`.** The Q12 VM is
single-threaded and `!Send`: all Lua runs on one worker thread. Shared-dict
state therefore lives in `SharedDictRegistry` — a `RefCell<HashMap<String,
Rc<RefCell<SharedDictStore>>>>` placed in the VM's `app_data` — and is borrowed,
not locked. Scope is **worker-process**, not cluster-wide: values set by
request A are observable by request B on the *same* worker (verified by the T5.3
gate `shared_dict_persists_across_requests_real_tcp`), but do not cross
workers. This is the openresty `lua_shared_dict` contract exactly (openresty
shmem is likewise per-worker-process). No `DashMap`: it would imply
multi-threaded VM access that the Q12 model forbids.

**Decision C — hand-written LRU, no `lru` crate.** `resty.lrucache` is a
*local* (per-userdata, not shared) cache, distinct from `ngx.shared.DICT`.
Its ordering is a plain `Vec<Vec<u8>>` (MRU front) keyed by a `HashMap<Vec<u8>,
RegistryKey>`, where values are stored in the Lua registry (so tables, numbers,
and strings all work, matching openresty). No `lru`/`linked-hash-map`
dependency — consistent with the Q4 minimalism stance. Default capacity 1024
(openresty default).

**Decision D — Scope A / Scope B split.** T5.3 is delivered in two slices.
**Scope A (Sprint 7, this ADR):** `resty.lrucache`, `ngx.shared.DICT` (full
API: get/set/add/replace/incr/delete/flush_all/get_keys/get_all),
`resty.random` (bytes/token via `getrandom`), `resty.string` (base64/hex) and
`resty.sha256`. **Scope B (Sprint 8, deferred):** `resty.http`, `resty.lock`,
and the remaining digests (md5, sha1, sha512). The split exists because
`resty.http` needs cosocket TLS, which is blocked behind **T5.4/T5.6**.

**Consequences.**
- `+` Zero-config shared state: Lua `ngx.shared.<anything>:set/get` works today,
  unblocking T5.4 routing-table experiments with no `lua_shared_dict` plumbing.
- `+` Faithful openresty semantics: per-worker scope, the full DICT API, and
  `resty.lrucache` that accepts arbitrary Lua values (tables included).
- `+` The T5.3 acceptance gate is a *real TCP* round-trip — request A writes,
  request B reads, on the actual data plane (`serve.rs`), not a unit stub.
- `−` **Documented deviation:** accessing an undeclared dict *succeeds* instead
  of erroring. Operators cannot reserve a fixed size slot pre-runtime. To be
  tightened in **T5.4**: when the Ingress compiler emits config, it will
  pre-create well-known dicts (e.g. `limit_req`, `balancer_state`) with explicit
  capacities, and `__index` will honour an existing entry before auto-creating.
- `−` State is not cluster-replicated; multi-worker deployments see independent
  dicts. This matches openresty and is sufficient for v1 Ingress (counters are
  eventually-consistent across workers). Cluster sync is a post-v1 concern.
- → **T5.3 → in-progress (Scope A).** `resty::*` core lands; unblocks **T5.4**
  (M1 Ingress spike) for routing-table + rate-limit experiments. Scope B
  (resty.http/lock, TLS-bound) follows in Sprint 8.

---

## Q16 — The crypto provider: `ring` over `aws-lc-rs` (T5.4 Scope B / TLS)

**Date.** 2026-07-26 (Sprint 8).

**Context.** Scope B adds TLS in two places: **termination** (the reverse proxy
accepts HTTPS — SNI selects the cert, the rustls analog of openresty's
`ssl_certificate_by_lua`) and the **client side** (the cosocket `sslhandshake`
and `resty.http.request_uri` dial HTTPS upstreams). rustls 0.23 requires an
explicit crypto provider; the choice is `ring` vs `aws-lc-rs`.

**Decision — `ring`.** Every TLS dependency selects the `ring` provider and
disables default features:
- `rustls = { version = "0.23", default-features = false, features = ["ring", "std", "logging", "tls12"] }`
- `tokio-rustls = { version = "0.26", default-features = false, features = ["ring", "logging", "tls12"] }`
- plus `rustls-pemfile` (PEM parse), `webpki-roots` (Mozilla root store for the
  client `verify=true` path); the test-only self-signed-cert generator is
  `rcgen` (`ring` feature) — a dev-dependency, never shipped in the release
  binary (R5).

Rationale:
- **Smaller, widely audited** — the leaner provider, consistent with the
  minimal-dependency posture (ADR **Q4**) and the router's `#![forbid(unsafe_code)]`.
- **No extra C-toolchain build dependency** for the provider in the
  configurations used (keeps the offline/vendored build, Q6, simple).
- **One provider, everywhere** — server termination, the cosocket client, and
  `resty.http` all share one `rustls::crypto::ring::default_provider()` via
  `tls::build_server_config` / `build_client_config`.

**Consequences.**
- `+` A single, audited crypto stack across the data plane (no mixed providers);
  a cert that validates in one path validates in all.
- `+` `verify=false` (the `NoVerify` client verifier) is shared verbatim by the
  cosocket `sslhandshake` and `resty.http`, so Lua that disables verification
  behaves identically in both (deliberate M1 parity).
- `−` `aws-lc-rs`-only cipher suites are unavailable; none are needed for the
  M1 Ingress/TLS slice.
- `−` Dynamic, Lua-driven cert issuance (openresty's true mid-handshake
  `ssl_certificate_by_lua` callback) is **deferred** — rustls's
  `ResolvesServerCert` is exactly the synchronous SNI selection point, so the
  `SniCertResolver` seam is already in place for a future Lua callback.
- → unblocks **T5.4 Scope B** (HTTPS termination + client TLS) and the
  `resty.http`/`resty.lock` work, whose TLS paths depended on this.

## Q17 — Storage backend strategy: pure-Rust embedded store vs etcd FFI vs supervised etcd (T2.1)

**Date.** 2026-07-27 (Sprint 10).

**Context.** T2.1/T2.2 must give the apiserver a place to persist objects.
Upstream k3s delegates to a real etcd (or to its SQLite `kine` shim for the
embedded-server mode) and speaks the etcd **gRPC v3** API. init-pro has three
honest ways to acquire that capability for v1: (A) link against Go's `etcd`
via cgo/FFI (or a Rust etcd-client against a bundled etcd *subprocess* we
supervise), (B) supervise a real `etcd` binary as a child process (mirrors how
k3s bundles `etcd` in the tarball, Q6), or (C) implement the **etcd
semantics** that Kubernetes actually depends on in pure Rust behind a trait,
treating etcd-the-database as one swappable implementation. Q10 already
committed JSON-only storage encoding and Q1 the single-multicall-binary
posture, both of which bear on the choice.

**Options.**
- A) **etcd FFI / link Go `etcd`.** Highest fidelity but pulls a Go runtime into
  a Rust binary via cgo, fights `#![forbid(unsafe_code)]`, and either bloats the
  image (Q6 size budget) or forces an async Rust↔Go-runtime bridge with a large
  unsafe/concurrency hazard surface. There is no maintained *pure-Rust* etcd
  server implementation to link against — only clients.
- B) **Supervise a bundled `etcd` subprocess.** Matches k3s's bundle-and-run
  ethos (Q6) and the existing `vendor` acquire path. But it re-introduces the
  full etcd-gRPC wire surface, a second process lifecycle to manage (graceful
  shutdown, restart, auth), and a ~25 MB Go binary in the image — and for the
  *embedded/single-server* mode it still needs an on-disk store, i.e. much of
  option C anyway.
- C) **Pure-Rust embedded store behind a `StorageBackend` trait; etcd-gRPC and
  SQLite/KINE become alternative trait impls.** The apiserver depends only on
  the trait. A zero-dependency in-memory `EmbeddedStorage` implements the
  *etcd semantics Kubernetes actually relies on* (a single monotonic cluster
  revision; per-key `create_revision`/`mod_revision`/`version` matching etcd's
  `KeyValue`, where `mod_revision` *is* the Kubernetes `resourceVersion`;
  optimistic concurrency via an `if_revision` compare; live `watch` via
  broadcast fan-out). `--datastore-endpoint` selects the impl: embedded by
  default, a real etcd gRPC client or a SQLite/KINE impl later.

**Decision: C.** Implement etcd *semantics* in pure Rust behind the
`StorageBackend` trait; do **not** FFI-link Go etcd or supervise a bundled
subprocess for v1. The `crates/storage` crate ships the trait + the embedded
default impl. This is the spike recorded against T2.1. `--datastore-endpoint`
is the switchover seam (Q5-style): pointing it at an `etcd://` URL selects a
real-etcd gRPC client impl (T2.3), and a `sqlite://`/KINE path selects the
on-disk single-server store; the apiserver code is unchanged across all three.

**Consequences.**
- `+` Keeps `#![forbid(unsafe_code)]` intact (no cgo, no process supervision of
  a Go binary), consistent with Q4/Q6's minimal, audited dependency posture.
- `+` The apiserver (T1.2) programs to a *trait*, not a wire protocol — so
  persistence lands on the critical path without an etcd-gRPC client being on
  it. Swapping in a real backend later is a trait-impl swap behind an
  endpoint flag, not an apiserver rewrite.
- `+` The embedded store is the perfect test double: T1.2 unit/integration
  tests get deterministic, in-process persistence with no port/socket teardown.
- `+` Faithful *revision/CAS/watch* semantics mean Kubernetes-level invariants
  (resourceVersion monotonicity, optimistic-concurrency `409 Conflict`,
  watch-from-revision) hold identically across backends.
- `−` The embedded store is **in-memory, per-process, non-durable**: it is the
  default and the test double, *not* an HA/durable production backend. Loss on
  restart is expected until T2.3 (SQLite/KINE) or a real-etcd impl lands.
  *(Superseded in Sprint 19 / T2.3 closeout: restart persistence is delivered
  opt-in by the libsql/SQLite backend (`sqlite://<path>`); the embedded store
  stays the in-memory default — see Q29.)*
- `−` The embedded store does **live-watch only** (no historical replay from a
  past revision); resource-version-based historical replay is an
  etcd-gRPC-backend capability, deferred to T2.3. *(Superseded in Sprint 12
  / T2.2 closeout: replay + compaction now ship in `EmbeddedStorage` behind
  the same trait — see `crates/storage/src/history.rs`. Only durability and
  the etcd-gRPC client remain T2.3.)*
- `−` Multi-server HA (leader election, raft) is **out of scope for the embedded
  impl** entirely; it arrives with the real-etcd backend and T3.4
  (multi-server).
- → unblocks **T1.2** (the next gate), which now programs to
  `StorageBackend`; T2.3 slots alternative impls in behind
  `--datastore-endpoint` with no apiserver churn.

**Partially superseded (Sprint 19 / T2.3, 2026-08-17).** Everything above
stands — the trait abstraction, the embedded in-memory default, the no-FFI/
no-subprocess posture — except the "real etcd-gRPC client someday" tail:
that client's only consumer is HA multi-server (T3.4), so it is re-scoped
to T3.4 (previously parked under T2.3). Durability itself is now delivered
opt-in by the libsql/SQLite backend (`--datastore-endpoint
sqlite://<path>`, Q29); the embedded store remains the default and the
test double. See Q29.

---

## Q18 — Leader election without etcd leases: coordination.k8s.io Lease + resourceVersion CAS (T3.1)

**Context.**
plan/03 originally sketched "leader election via etcd lease (T2.2)" for the
controller loops. That couples election to one backend capability (etcd TTL
leases) that the embedded pure-Rust store (Q17) deliberately does not
implement — and T3.1 must run on the embedded backend.

**Options.**
1. Implement etcd-lease semantics (TTL, keep-alive, expiry) in the embedded
   backend, then lease-based election.
2. Upstream semantics: leader election via `coordination.k8s.io` `Lease`
   objects — acquire = optimistic CAS on `spec.holderName` +
   `resourceVersion`, renew = periodic update, all plain storage operations
   every backend already has. This is exactly what client-go's
   `leaderelection` does on top of the API.
3. No election in v1 (single-server only) — but T3.4 needs the seam anyway.

**Decision.**
Option 2. Elections are expressed as ordinary API resources + CAS, never as
backend-private primitives. `StorageBackend` does not grow leases.

**Consequences.**
- `+` Backend-agnostic: works identically on the embedded store (v1), KINE/
  SQLite, and real etcd (T2.3+), because it only uses create/update/CAS.
- `+` Upstream-faithful: `kubectl get leases -n kube-system` shows real
  election state; client-go tooling interoperates.
- `+` Keeps the trait minimal (Q17); no TTL timers or keep-alive loops
  inside the storage layer.
- `−` Lease expiry is cooperative (renewal deadline checked on read/update),
  not hardware-timed like etcd TTLs — acceptable for v1; T3.4 revisits
  fencing if HA demands stronger guarantees.
- → plan/03-control-plane.md T3.1 updated accordingly.

---

## Q19 — Controller-manager transport: in-process storage trait vs HTTP client (T3.1a)

**Date.** 2026-08-14 (Sprint 13).

**Context.**
T3.1 controllers need a Kubernetes API client. Upstream controllers talk
HTTPS to the apiserver; init-pro v1 is a single binary where apiserver +
controllers share one process and one storage `Arc` (Q1, Q17).

**Options.**
- A) **HTTP client loopback through the apiserver.** Wire-faithful and it
  exercises the HTTP stack — but it needs auth (T1.3 not started), TLS
  (deferred per Q11), serialization overhead per hop, and adds a
  self-dependency loop at boot (controllers ⇄ apiserver ⇄ controllers).
- B) **In-process `Client` trait over `Arc<dyn StorageBackend>` shared with
  the apiserver.** Same object store, same CAS/watch semantics, zero
  network.
- C) **Both behind a trait; the HTTP impl is deferred.**

**Decision.**
C-shape now: define the small `Client` trait in the controllers crate, ship
the `StorageClient` (in-process) for v1; the HTTP-backed implementation
arrives with T3.4 (HA / out-of-process controllers) and T1.3 (auth) — swap
at the trait boundary.

**Consequences.**
- `+` Zero boot-order/auth coupling: the manager starts whenever the
  storage `Arc` exists, no sockets required.
- `+` Tests run the full manager against `EmbeddedStorage` in-process,
  without sockets (deterministic, fast — 14 controllers integration tests).
- `+` The trait keeps T3.4 honest: out-of-process controllers are an impl
  swap, not a rewrite.
- `−` Controllers bypass apiserver admission/validation — no such layer
  exists yet in init-pro; when T1.3+ adds admission, the HTTP client or
  in-process admission hooks must be revisited.
- `−` `resourceVersion` semantics must be projected identically to the
  wire (`client.rs` does this explicitly).
- → plan/03-control-plane.md T3.1 records the as-built transport.

---

## Q20 — Garbage collection + namespace lifecycle semantics (T3.1b)

**Date.** 2026-08-14 (Sprint 14, slice S5).

**Context.**
T3.1b must define what DELETE means: ownerReferences cascade
(Deployment → ReplicaSet → Pod), `propagationPolicy` (Background /
Foreground / Orphan), finalizer-gated soft delete, and Namespace
termination. In-process controllers bypass the apiserver (Q19), so the
delete bookkeeping cannot live only on the wire layer.

**Options.**
- A) **Upstream-faithful solid/weak owner graph with uid tombstones +
  foreground blocking deletion.** Full parity, but needs uid assignment,
  foreground finalizer injection, and a graph re-solver — heavy for v1
  where nothing assigns uids yet.
- B) **Declarative drain only** (each controller deletes its own children
  on owner delete). No orphan support, no dangling-ownerRef cleanup, and
  namespace teardown would need per-controller special cases.
- C) **Simplified GC: cache-verified owner existence + annotation-marked
  Orphan + namespace controller drain.** DELETE of a finalizer-carrying
  object soft-deletes (deletionTimestamp); hard DELETE with
  `propagationPolicy=Orphan` stamps `init-pro.io/deletion-propagation` on
  the way out so the DELETE event carries it; the controllers' GC sweeps
  dependents whose *managed* controller owner is absent from its informer
  cache (event-driven on owner DELETE + 2s periodic backstop); a namespace
  controller drains every namespaced kind then performs the terminal
  delete itself (finalizer completion on PUT/PATCH lives in the apiserver
  for kubectl-driven flows).

**Decision.**
C. Background cascade is the default; Foreground is treated as Background
in v1; Orphan strips matching controller ownerReferences (kind+name, uid
when set). Managed owner kinds: Deployment, ReplicaSet, Service,
StatefulSet, DaemonSet. Unknown owner kinds are skipped (upstream GC only
acts on owners it can verify).

**Consequences.**
- `+` kubectl-visible semantics land (soft delete, finalizers, namespace
  `kubernetes` finalizer injection, orphan keeps dependents) without the
  uid/tombstone machinery.
- `+` GC is event-driven (owner DELETE hooks) with a cheap 2s backstop;
  terminating namespaces always progress via the backstop re-enqueue.
- `−` Accepted v1 race: a just-created owner may be briefly absent from an
  informer cache, so a dependent can be deleted and recreated by its
  controller — self-healing, no durable divergence.
- `−` In-process controllers own the namespace terminal delete (they
  bypass the apiserver's finalizer-completion rule); T3.4's HTTP-client
  controllers will route through the apiserver instead.
- → plan/03-control-plane.md T3.1b records the as-built GC/namespace
  controller set.

## Q21 — kubectl rollout status transport: plain poll loop vs watch stream (T3.1b)

**Date.** 2026-08-14 (Sprint 14, slice S2).

**Context.**
`kubectl rollout status` must be exit-code compatible with upstream
(0 = rolled out, 1 = NotFound / progress deadline exceeded). Upstream
kubectl holds a **watch** on the Deployment and prints waiting messages
as they change. Our `kubectl` crate speaks plain HTTP with JSON-only
wire (**Q10**) and has no watch-stream client.

**Options.**
- A) **Watch-stream client inside the kubectl crate.** Event-fresh, but
  duplicates the apiserver watch framing (SSE/chunked parsing) into a
  second crate for one subcommand.
- B) **Pure poll loop (GET every 250ms) + a pure `evaluate` function**
  implementing the rollout-status semantics; print each NEW waiting
  message once; map the final state to an exit code.
- C) **Reuse the controllers informer.** Wrong boundary: kubectl is a
  standalone client process and must not link controller machinery.

**Decision.** B — `crates/kubectl/src/rollout.rs` splits the problem:
`evaluate` is a total function over the Deployment JSON
(observedGeneration lag, ProgressDeadlineExceeded, complete, waiting
messages — everything defaults, never panics on odd input), and
`rollout_status` drives the Q21 poll loop. NotFound and deadline map to
exit 1; "successfully rolled out" to exit 0.

**Consequences.** No long-lived connections through proxies; ≤250ms
message latency, invisible for a human-facing command; the pure
`evaluate` core is directly unit-testable and watch-freshness can be
added later without touching semantics.

## Q22 — StatefulSet storage: real PVC + ControllerRevision objects, lazy binding until T6.2 (T3.1b)

**Date.** 2026-08-14 (Sprint 14, slice S3).

**Context.**
StatefulSet identity requires per-ordinal PVC names
(`<claim>-<sts>-<ordinal>`) that survive scale-down, plus
ControllerRevision history so rollout/rollback has an auditable record.
Real k3s has a PV binder; init-pro has no kubelet/CNI yet (T4.2/T4.3)
and no PV controller (T6.2).

**Options.**
- A) **Virtual PVCs** synthesized at read time. No storage footprint,
  but PVCs become invisible to REST list/get and to future binders.
- B) **Real PVC objects created by the controller, left Pending**;
  ControllerRevisions stored as real `apps/v1` objects
  (`<sts>-<hash10>`, bumped `revision`).
- C) **Ship a minimal binder now.** Premature: nothing consumes bound
  volumes until T4.x.

**Decision.** B — the controller materializes one PVC per
claim-template per ordinal below `spec.replicas`, never deletes them on
scale-down (retention = Retain, v1), and writes every distinct pod
template as a ControllerRevision object.

**Consequences.** PVCs/ControllerRevisions are listable via REST and binder-ready;
`status.phase` stays Pending until T6.2 (documented v1 default);
controller state stays stateless across restarts because history lives
in storage, not memory.

---

## Q23 — kube-scheduler v1: controllers-framework reuse, logical nodes, HTTP-only extenders (T3.2)

**Date.** 2026-08-15 (Sprint 15).

**Context.**
T3.2 needs a kube-scheduler equivalent inside the single binary. Upstream
ships a plugin framework (Filter/Score/Reserve/Bind extension points) with
default plugins, plus an HTTP extender protocol; k3s runs the stock
scheduler. init-pro had a working informer/workqueue/leader-election
framework from T3.1 (the `controllers` crate), no kubelet (so Nodes may
carry no `status.capacity`), and needs an AI-agent policy seam for Layer 7
(Q3).

**Options.**
- A) **Third framework copy** — a scheduler-private client/informer stack.
  Duplication of ~600 tested lines for no new behavior.
- B) **Embed kube-scheduler / a Rust port of the whole framework**
  (Reserve/Permit/PreBind/Binder extension points, profile config).
  Massive surface for a v1 with no volume/permit use.
- C) **Reuse the controllers crate's machinery; a small pure-function
  plugin layer (Filter + Score + Bind) with the default plugins; upstream
  wire-shape HTTP extenders only.**

**Decision.** C, with three v1 semantics pinned:
1. **Framework reuse** — `SchedulerManager::spawn` mirrors
   `ControllerManager::spawn` (Q19 in-process storage, Q18 Lease+CAS
   election under `init-pro-scheduler`, pods/nodes/PVCs informers, one
   pending-pod workqueue, 2 workers). Extenders are **HTTP-only**
   (`TcpStream` + chunked decode, kubectl-Q21 pattern; no gRPC dep);
   `https://` URLs are rejected in v1.
2. **Logical nodes** — a Node without `status.allocatable`/`capacity`
   is treated as **unbounded** (logged once per node name): v1 test /
   headless clusters register bare Node objects. ResourceFit passes them
   and LeastRequested scores them a flat 100.
3. **Anti-oscillation** — `Unschedulable` is written write-if-changed
   (`semantic_eq` + preserved `lastTransitionTime`); a pod is re-enqueued
   **only** by pod/node events or a 30 s backstop sweep, never by the
   worker loop (the Sprint-13 quiesce pattern; covered by a dedicated
   integration test asserting revision stability).
Extenders wire through the real k3s flag `--kube-scheduler-arg
config=<KubeSchedulerConfiguration.json>` (moved from the no-op table to
the wired surface); the extender JSON matches upstream field names
(`urlPrefix`, `filterVerb`, `prioritizeVerb`, `weight`, `ignorable`,
`nodeCacheCapable`). The apiserver also serves
`pods/{name}/binding` (201/404/409/422 semantics) so both in-process and
HTTP clients bind identically.

**Consequences.**
One framework to maintain; the plugin layer is pure
`(pod, node, snapshot) -> verdict|score` (repo rule: no OOP state).
Deferred vs upstream: inter-pod affinity (only anti-affinity in v1),
volume binding (v1 passthrough honors a PVC `spec.nodeAffinity`
init-pro extension), Permit/PreBind phases, profile config, score
weighting config. G22/G23 gate the behavior (23/23 golden).

## Q24 — T4.1 spike: vendored containerd is supervisable through the multicall seam (timeboxed)

**Date.** 2026-08-15 (Sprint 15, timeboxed risk spike — T4.1 stays in-progress).

**Context.**
T4.1 (containerd bundling + CRI) is the longest unstarted chain to M3
(T4.1 → T4.2 → T4.3/T4.5). The riskiest unknowns: do the pinned artifacts
(containerd 1.7.20, runc 1.1.13, cni-plugins 1.5.1) actually run under the
multicall/exec model, and can the k3s-style supervise + CRI socket be
reached without new runtime deps?

**Options.**
- A) **Start T4.1 properly** (config templating + supervisor in the
  server runtime). Blocks T3.2 sequencing and exceeds the sprint.
- B) **Timeboxed spike**: stage → config → boot via `init-pro containerd`
  → verify daemon/CRI/`ctr`, record findings, leave T4.1 in-progress.

**Decision.** B. Findings (all verified by
`scripts/t41-containerd-spike.sh`, 5/5):
- The vendored bundle **boots through the multicall seam**: peer aliases
  `containerd`/`ctr` now re-exec `<data-dir>/agent/containerd/<name>`
  when staged (Q1 symlink deployment; `INIT_PRO_DATA_DIR` locates the
  data dir; crictl stays a stub — not vendored by decision).
- k3s layout works as-is: binaries under `agent/containerd/`, config
  under `agent/etc/containerd/config.toml`, CNI conf `agent/etc/cni/net.d`,
  cni-plugins in `agent/containerd/aux`, socket `<dd>/run/containerd/containerd.sock`.
- A ~20-line TOML (`version = 2` + CRI plugin block) yields
  `io.containerd.grpc.v1 cri … ok`; `ctr version` round-trips through the
  seam; runc 1.1.13 loads as `io.containerd.runc.v2`.
- Egress: ghcr.io reachable; docker.io / registry.k8s.io unreachable from
  this environment — the image-pull smoke is best-effort and skipped
  (Q: airgap mode implications for T4.2 e2e; SBOM/license gate already
  passed per T0.2).
Remaining for full T4.1: config templating in the server runtime,
supervisor with restart/backoff, `crictl` decision (pin vs CRI-only),
image pre-pull/airgap strategy, CRI client wiring for T4.2.

**Consequences.**
The riskiest T4.1 assumption (supervising a foreign runtime through the
multicall binary) is de-risked with ~90 lines of script + a 30-line seam
in `multicall`. T4.2 unblocks once T4.1's runtime wiring lands; no new
workspace dependencies were added.

## Q25 — T4.1 production wiring: Rust-native runtime crate, vendored crictl, sticky shutdown (T4.1)

**Date.** 2026-08-15 (Sprint 16; supersedes Q24's "crictl stays a stub"
note — T4.1 now vendors crictl).

**Context.**
Q24 proved vendored containerd is supervisable through the multicall
seam. T4.1 proper must turn the spike into production wiring: config
templating without shell scripts, supervision with restart/backoff, a
real `crictl` (acceptance is literally `init-pro crictl ps`), and clean
integration with the agent CLI + graceful shutdown.

**Options.**
- A) **k3s-style shell wrappers** around the vendored tree. Hidden /bin/sh
  dependency, unfollowable in `#![forbid(unsafe_code)]` audit, breaks the
  single-binary story.
- B) **Rust-native `runtime` crate** (config + stage + supervisor),
  crictl vendored through the existing T0.2 pin/verify pipeline,
  crictl/ctr passthrough via the multicall seam.

**Decision.** B. As-built:
- New `runtime` crate. `config.rs`: `ContainerdConfigVars::for_data_dir`
  renders TOML v2 (CRI plugin enabled); `sandbox_image` overridable via
  `INIT_PRO_SANDBOX_IMAGE` (default `registry.k8s.io/pause:3.10`). Unit
  tests parse the render back with `toml` (dev-dep only).
- `stage.rs`: idempotent SHA-256 staging of the containerd tree
  (containerd, ctr, shims, runc, crictl, `aux/` cni-plugins), CNI
  loopback conflist `10-init-pro.conflist`. `vendor_bin_root` resolves
  `INIT_PRO_VENDOR_BIN` → exe-relative `../../vendor/bin` → cwd.
- `supervisor.rs`: spawn → socket-health gate (UnixStream poll) →
  exponential backoff `base << restarts` capped 5s; STABLE_AFTER 30s
  resets the ladder; drain = SIGKILL + bounded 10s wait. Deliberately
  **no SIGTERM step**: containerd's child reaping is not guaranteed for
  a foreign runtime, and k3s kills the tree too.
- `infra::signal::Shutdown` made **sticky** (fired flag +
  `Notified::enable`): the old memoryless `notify_waiters` could lose
  wakeups during select gaps; regression test added, all 29 infra tests
  green. This is a correctness fix for every Shutdown consumer.
- CLI: agent branch calls `start_agent_runtime`; runtime drains FIRST on
  shutdown. `init-pro crictl …`/`init-pro ctr …` are intercepted
  pre-clap and re-exec the staged peer with endpoint injection
  (`--runtime-endpoint` / `--address`) unless the user supplied one.
- `crictl` v1.31.1 pinned in `vendor/versions.toml` (SHA-256 verified
  against the official `.sha256`); staged like every other peer.
- Acceptance: `init-pro crictl ps` / `pods` / `version` round-trip over
  the CRI socket (live smoke + `crates/runtime/tests/supervisor_integration.rs`
  incl. kill -9 → rebirth); sandbox-image pull smoke is best-effort SKIP
  (registry auth-gated in this environment; see Q24 egress note).

**Consequences.**
The agent now owns a real node runtime and `crictl` is a first-class ops
verb; T4.2 (kubelet) inherits the socket + config seam. Debt recorded
for T4.2: no image pre-pull/airgap store yet, no cgroup delegation, and
drain is kill-first (acceptable for v1, revisit if containerd adds
graceful drain hooks). Sticky `Shutdown` changes semantics for all
consumers — strictly more correct (idempotent, no lost wakeups).

## Q26 — CRI client strategy: vendored-crictl subprocess now, native gRPC later (T4.1 → T4.2)

**Date.** 2026-08-15 (Sprint 16, timeboxed dual-route spike; conclusions
only — spike lived outside the workspace, no new deps).

**Context.**
T4.2 (kubelet) and golden gates need CRI calls. Two viable routes,
measured in Sprint 16 against a live agent's CRI socket:

**Options.**
- A) **Native gRPC client** (tonic 0.12 `transport+codegen+prost` +
  prost 0.13, hand-rolled message structs, UDS connector via
  `tower::service_fn` + `hyper_util::rt::TokioIo`): measured
  `runtime.v1.RuntimeService/Version` round-trip **sub-millisecond**;
  **100 unique crates** in the dep tree; ~15s cold build for just that
  subset. Full CRI surface (runtime + image services, ~dozens of
  messages, streaming Exec/Attach, PodSandboxEvents watch) needs
  vendored `cri-api` protos + tonic-build + protoc — protoc is NOT
  present in this environment — or maintained hand-rolled structs with
  field-number drift risk. Also: `tonic::client::Grpc` requires
  `ready().await` before `unary` (buffer layer panics otherwise).
- B) **Subprocess through the vendored crictl**: zero new deps,
  **~20ms/call** (process spawn dominated), covers ps/pods/images/pull/
  run/exec incl. `exec` stdio passthrough; gaps: per-call spawn cost, no
  watch/stream subscriptions, output is text/JSON not typed structs.

**Decision.** B now, A later. crictl is the ops verb and the T4.2
interim client. Introduce a native gRPC client only when T4.2 actually
needs in-process streaming/watch (sandbox events, pull progress,
port-forward), scoped to a leaf `cri-client` crate behind a feature flag
so the 100-crate dep cost is opt-in. The UDS-connector + sticky-shutdown
patterns from the spike are recorded above for that day.

**Consequences.**
No new workspace dependencies in Phase 2; `init-pro crictl` gives
kubectl-parity node ops today with proven semantics (Q25). The trigger
for route A is explicit (streaming/watch need), not convenience. When it
fires, budget for vendoring `cri-api` protos and a build-time protoc
strategy (or prost-build with a checked-in prebuilt descriptor set).

## Q27 — Airgap workload image: local OCI assembly for the pause sandbox (T4.2)

**Date.** 2026-08-16 (Sprint 17, T4.2 Scope A).

**Context.**
G25 needs a real workload image to drive a pod to Running+Ready. Every
registry route is closed in this environment: registry.k8s.io redirects
to a blocked `europe-west4-docker.pkg.dev` CDN, docker.io is blocked
outright (cf. the Q24 egress note), and ghcr.io has no pause repo. The
sandbox image must arrive with zero network.

**Options.**
- A) **Vendor a pause image tarball** into the repo/build. Commits a
  binary blob to git, grows the checkout, and still needs an import
  step at the agent.
- B) **Build a minimal pause locally** at deploy/test time into a
  hand-assembled OCI layout, then import it through the staged `ctr`.
- C) **Runtime HTTP pull from a mirror.** Requires a reachable mirror
  (none exists here) and reintroduces network dependence into the
  golden path.

**Decision.** B. As-built:
- `scripts/build-pause-image.sh` compiles a static pause binary (gcc
  `-static -Os`) and hand-assembles an OCI image layout — no registry
  round-trip anywhere. Ref `init-pro.local/pause:0.1`.
- Imported through the staged `ctr` into namespace k8s.io
  (`INIT_PRO_DATA_DIR=<dd> init-pro ctr -n k8s.io images import <tar>`);
  the agent honors `INIT_PRO_SANDBOX_IMAGE` (the Q25 seam), started
  with `init-pro.local/pause:0.1` for G25.

**Consequences.**
Zero network + zero binary blob committed — the image is reproducible
from source at image-build time. Needs `cc` at image-build time;
golden SKIPs G25 without it, exactly like G24's vendor gate. The same
script is the template for future airgap workload images. Registry
pulls (the Q26-era `crictl images pull` path) remain for connected
environments.

## Q28 — Service traffic: NodePort-only plane in the built-in Router (no ClusterIP dataplane) (Sprint 18)

**Superseded (Sprint 51, 2026-11-22).** Q44 (T4.3 Scope C / S51,
LOCKED) supersedes ONLY Q28's ClusterIP-deferred semantics — the
`--service-cidr` noop and "ClusterIP Services stay creatable but
non-forwarding" are void as of S51 (分配面归 apiserver, 承载面归 Router
`clusterip.rs`). The NodePort plane itself and the rest of Q28 remain in
force. See Q44.

**Date.** 2026-08-17 (Sprint 18, recorded at S7 — "decision D" in the
slice commits).

**Context.**
Sprint 18 made pods reachable end to end: the kubelet emits real podIPs
from the CRI sandbox (S1), Endpoints resolve targetPort and carry those
podIPs (S2), the apiserver auto-allocates nodePorts (S3), and
`scripts/local-up.sh` boots a real single-node cluster (S0). The
cluster now needs a Service dataplane — something must carry traffic
from a stable Service entry point to the live pod set. k3s runs
kube-proxy (iptables/IPVS) plus servicelb for this. Q4 already commits
to the built-in Router as a first-class platform primitive, and the
sprint's acceptance (S6, `scripts/service-traffic-e2e.sh`) needs real
HTTP traffic through a Service on this machine, where no netns/iptables
dataplane machinery exists yet.

**Options.**
- A) **Full kube-proxy equivalent** — ClusterIP iptables/IPVS rules
  plus NodePort/LoadBalancer handling. The faithful k3s port, but a
  large netns/iptables/nftables surface that no current test needs;
  ClusterIP forwarding is pointless before pod-to-pod networking
  outgrows the single-node bridge anyway. Defers the visible value
  (Service traffic that works) behind weeks of kernel-rule machinery.
- B) **NodePort-only plane in the built-in Router** — one reverse-proxy
  listener per allocated nodePort, upstreams resolved from live
  Endpoints; ClusterIP Services stay creatable/storable but
  non-forwarding (`--service-cidr` remains a noop). Reuses the proven
  T5.4 proxy/balancer machinery, pure userspace, testable in-process
  and end to end.
- C) **Delegate to an external kube-proxy/CNI plugin** (supervised
  upstream binary). Contradicts Q4 (first-class built-in Router) and
  Q1 (single multicall binary); adds a dependency absent from
  `vendor/versions.toml` with no offline source in this environment.

**Decision.** B. As-built (Sprint 18, S4):
- `crates/router/src/endpoints.rs` + `endpoints_watch.rs`: LIST→WATCH
  reflectors over services + endpoints on the SAME embedded-storage
  `Arc` the apiserver uses (gap-free, revision-ordered, informer-style
  re-LIST on stream close), folded into a `ResolverState` that
  implements T5.4's `UpstreamResolver` (numeric/named/identity
  targetPort; ns/name + bare-name lookup).
- `crates/router/src/nodeport.rs`: one reverse-proxy listener per
  allocated nodePort of every **NodePort-type** Service, managed by
  a reconcile loop on a dedicated worker thread (current-thread runtime
  + `LocalSet`, matching `serve_proxy`'s spawn_local model); Endpoints
  updates re-target without restart, Service delete retires the
  listener, shutdown drains the plane.
  **As-built fact (Sprint 19 review):** `nodeport.rs` matches
  `kind_type == "NodePort"` only — LoadBalancer-type Services get
  nodePorts allocated from the same allocator but have **no listener**;
  the LoadBalancer plane (external VIP + L4) is the remaining T5.6
  scope, gated on T4.3.
- Wired in `crates/cli/src/runtime.rs`. The plane is **on by default**
  in the server; `--disable-kube-proxy` (a pre-parsed noop until this
  sprint, kept for k3s flag parity) turns it off. The Router gained
  `infra` + `storage` workspace dependencies only.

**Consequences.**
Service traffic works end to end on a local cluster today
(`scripts/local-up.sh` + `scripts/service-traffic-e2e.sh` ST1–ST6, a CI
gate that SKIPs itself — exit 0 — without a vendor bundle or `cc`,
G24/G25 parity). Polarity is deliberate: plane ON by default,
`--disable-kube-proxy` opts out, zero config churn otherwise. Empty
Endpoints answer **503** (not connection reset), so scale-to-zero is
observable (ST5); Service delete retires the listener (ST6: refused).
LoadBalancer-type Services get nodePorts from the same S3 allocator but
no external VIP yet — VIP announce is the remaining T5.6 scope and
needs the T4.3 dataplane. Deferred to future work: the ClusterIP
dataplane (`--service-cidr` stays a noop; ClusterIP Services are
creatable/storable but non-forwarding), LoadBalancer external VIPs,
sessionAffinity, UDP, externalTrafficPolicy. If a ClusterIP dataplane
is ever required it lands as a Router/T4.3 extension, not a forked
iptables kube-proxy — Q4's single-dataplane story holds.

---

## Q29 — Durable storage: libsql/SQLite backend, local-file only, opt-in via `--datastore-endpoint` (T2.3 / Sprint 19)

**Date.** 2026-08-17 (Sprint 19, recorded at S6).

**Context.**
Layer 2 closed its last gap with T2.3: the embedded store is in-memory,
so every boot was a fresh cluster. k3s solves single-server durability
with kine — a SQLite (or dqlite/postgres) shim exposing the etcd v3
API surface. init-pro already has the etcd *semantics* behind the
`StorageBackend` trait (Q17), so the task reduces to a second trait
impl over a durable local-file substrate, selected by the existing
`--datastore-endpoint` flag (k3s parity). The async Rust SQLite
landscape offered three honest candidates, and the choice had to keep
`#![forbid(unsafe_code)]` in our crates, avoid a network stack on the
critical build, and stay inside the caret-only dependency contract.

**Options.**
- A) **libsql with `default-features = false, features = ["core"]`.** The
  SQLite fork kine-adjacent tooling uses, with a native async API and no
  tokio blocking bridge of our own. Core-only mode compiles out the
  remote/replication/sync/tls machinery (no hyper/prost/tonic on the
  critical build); the FFI lives behind libsql's own `-sys`/`-ffi`
  crates, outside our `#![forbid(unsafe_code)]` surface. Maintainer
  decision: libsql is a hard requirement.
- B) **rusqlite + `spawn_blocking` wrapper.** The zero-async-native
  classic. Rejected: every call becomes a threadpool hop, watch fan-out
  ordering ("broadcast strictly post-COMMIT under one connection
  mutex") gets racy to reason about, and it buys nothing libsql core
  doesn't already provide locally.
- C) **sqlx (compile-time checked queries) or full libsql with remote
  features.** sqlx pulls a macro/compile-time database harness for a
  schema this small; full libsql drags the Turso remote/replication
  stack (hyper/prost/tonic + auth) that single-node durability does not
  need onto every build.

**Decision.** A, as built (Sprint 19):
- `crates/storage/src/sqlite/` — `SqliteStorage` on libsql 0.9,
  core-only. Kine-style schema: one append-only `kv` event table
  (row `id` = global revision AUTOINCREMENT; key; value as JSON TEXT;
  `create_revision`; `prev_revision`; `deleted`; `version`) plus a
  `meta` table (schema_version / revision / compact_revision). PRAGMAs
  `journal_mode=WAL`, `synchronous=FULL`, `busy_timeout=5000ms`; the
  revision counter is restored at open from max(meta.revision,
  MAX(kv.id)).
- Hybrid watch: in-process broadcast fan-out (cap 1024, `WATCH_CAP`
  parity with the embedded store) sent strictly **post-COMMIT** under a
  single connection mutex — broadcast order == commit order == revision
  order — while historical replay is plain SQL
  (`SELECT id >= start ORDER BY id`), so replay survives restart.
  Compaction keeps the latest row per key (get/list unaffected) and
  persists the watermark to `meta`; watch at/below the watermark →
  `StorageError::Compacted` (410 Gone). The backend assumes a single
  writer on a single connection, consistent with the Q17/Q18
  single-server posture.
- Explicit `version` column (etcd `KeyValue.version` parity: create=1,
  update=prev+1, tombstone carries the final version) — replacing a
  COUNT-derived value that shrank after compaction (mid-sprint defect).
- Default backend **unchanged**: empty/`None` `--datastore-endpoint`
  keeps `EmbeddedStorage` (Q17 opt-in posture); `sqlite://<path>`
  selects `SqliteStorage`; unsupported schemes are fatal, classified by
  the pure `classify_dsn`. Flipping to sqlite-by-default is a possible
  future decision, explicitly NOT taken here.
- The contract suite doubles as the parity gate: the 26 embedded tests
  were extracted into `crates/storage/tests/contract/` — 26 portable
  cases instantiated for BOTH backends via `storage_contract!`
  (1 embedded-only history-eviction case stays) — plus 5 file-backed
  durability tests and 8 unit tests.

**Consequences.**
A single-node cluster is now durable when the operator opts in:
restart preserves entries and revisions (resourceVersions identical
across restart), watch replays across restart, and the compaction
watermark survives — the "every boot is a fresh cluster" era ends
(`scripts/local-up.sh` boots `sqlite://$DD/server/state.db`;
`scripts/durability-e2e.sh` D1–D4 gates it in CI). Cost: +31 Cargo.lock
packages via libsql core (libsql/libsql-sys/libsql-ffi, bindgen, cmake,
futures, parking_lot, zerocopy, regex, ...), caret-only unchanged, and
a build-time cmake/cc dependency on the sqlite artifact (vendored-bundle
SKIP parity, as with G24/G25). The backend is local-file only by
design: no remote/replication/sync/tls, and no HA — multi-server
remains T3.4 with the real etcd-gRPC client (Q17 supersede note).
`--kine-tls`/`datastore-*` stay noops (flag-matrix §C.6). The embedded
store keeps its role as default and test double, so every existing test
and golden fixture runs unchanged.

---

## Q30 — Argo Workflows integration: typed built-in API group + registration-driven surface, not a vendored argo server (T7.3 / Sprint 21)

**Date.** 2026-08-25 (Sprint 21, T7.3 Scope A).

**Context.**
T7.3 makes argo-workflows a first-class built-in: `Workflow` and
`CronWorkflow` created and managed natively by init-pro, no separate
addon install. Upstream ships a Go API server (`argo server`), a Go
workflow controller, and the `argo` CLI — three subprocess-sized
surfaces. init-pro already has the generic machinery those need: a
typed resource model (`crates/api`), REST CRUD/SSA/watch routes driven
purely by schema registration (`crates/apiserver`, T1.2), control loops
(T3.1), and a kubectl-compatible client (`crates/kubectl`). The
question was which parts to port, which to reuse, and what to defer —
under the standing constraints: single multicall binary (Q1), full
protocol conformance (Q2), JSON-only wire (Q10), and no protobuf.

**Options.**
- A) **Vendored/supervised argo server + argo CLI** (subprocess bundling
  a la containerd, Q24-Q26). Rejected: `argo server` is a full Go
  apiserver+UI process — bundling it contradicts the Q1 single-binary
  story's spirit, adds a second apiserver in front of ours, and the
  upstream binary has no offline vendor slot in this environment.
- B) **Typed built-in API group + registration-driven surface.** Port
  the upstream v1alpha1 *types* faithfully into `crates/api`, register
  them like any built-in group, and let the existing generic routes and
  kubectl client serve them. No argo server, no bespoke handlers.
- C) **CRD-only, no typed port.** Register argoproj.io as an opaque
  schema and store blobs. Cheapest, but every downstream consumer
  (controller, kubectl rendering, validation, fixtures) loses the type
  information, and round-trip fidelity against upstream YAML becomes
  unverifiable.

**Decision.** B, as built (Sprint 21, Scope A):
- (a) Full-field typed 1:1 port of upstream v4 `pkg/apis/workflow/v1alpha1`
  into `crates/api/src/argoproj/` (16 modules, ~313 Go json tags).
  SSOT = upstream Go json tags from `link_repos/argo-workflows`;
  embedded k8s core types are reused from the `k8s_openapi` crate, not
  re-declared. Wire stays JSON-only (Q10): omitempty maps to
  `Option`/`Vec`/`BTreeMap` + skip-helpers, Go string enums become type
  aliases, custom serde exists only where upstream is genuinely polymorphic
  (`AnyString`/`Amount`/`Item`), steps are `Vec<Vec<WorkflowStep>>`,
  `WorkflowStep.inline` is `Option<Box<Template>>`, `Plugin` is a
  transparent `serde_json::Value`.
- (b) CRUD/SSA/watch come from the existing generic apiserver routes via
  schema registration — zero bespoke handlers. shortNames
  (workflows→`wf`, cronworkflows→`cwf`/`cronwf`) were added to the
  registry *contract* (`TypeInfo.short_names` +
  `SchemaRegistry::register_with_short_names()`; the old `register()` is
  unchanged) rather than hardcoded in discovery; discovery projects
  shortNames only when non-empty.
- (c) kubectl gets minimal built-in `get` / `apply -f` / `delete` verbs
  (verb-first dispatch, Output table, multi-doc YAML manifests) — not an
  argo CLI reimplementation. v3 manifests with the singular `schedule`
  field are accepted via a serde alias + string-or-vec deser and
  re-encoded as the v4 `schedules: Vec<String>`.
- (d) Workflow controller/executor semantics (actually reconciling
  Workflow specs into pods), artifact storage, events, and the wider
  argo CLI surface are deferred to later T7.3 scopes.

**Consequences.**
`argoproj.io` is a zero-install built-in: a fresh cluster lists
`Workflow`/`CronWorkflow` in `/apis` and `kubectl get wf` works out of
the box. The discovery surface is a byte-stable golden (**G27**:
`/apis/argoproj.io/v1alpha1`; **G28**: Workflow+CronWorkflow CRUD/SSA
lifecycle), and upstream YAML applies as-is (11 upstream-derived
fixtures + 8 round-trip fidelity tests pin this). No protobuf ever
(Q10); no argo server/CLI subprocess (Q1 holds). Cost: the typed port
must track upstream — its maintenance burden is bounded by upstream tag
drift, and the fidelity fixtures are the drift alarm. Until Scope B+
lands, argoproj.io objects are stored/served but nothing executes them;
the workflow controller is the explicitly open remainder of T7.3.

**Scope B addendum (Sprint 22).** Scope B landed the workflow
controller; the Q30 shape held, with these Scope B consequences:
- The controller runs **in-process** inside `ControllerManager`,
  reusing the T3.1 informer/workqueue framework over `StorageClient`
  (Q19; no HTTP client) — the same control-loop pattern as
  ReplicaSet/Deployment, not a supervised argo-controller subprocess
  (Q1 holds).
- Pods-as-steps WITHOUT the upstream wait/executor sidecar: the
  template container runs directly and kubelet terminal pod phases
  drive node phases (upstream-aligned terminal ladder: Never →
  Failed/Succeeded by exit code; OnFailure + all-zero → Succeeded).
- Node IDs are the node names; the upstream node-ID hash is elided.
- Unsupported template constructs (suspend/http/resource/plugin/missing
  template) park the workflow at `Pending` with a `status.message`
  naming them, rather than erroring or silently dropping steps.

**Scope C addendum (Sprint 23).** Scope C landed the CronWorkflow
controller and closed T7.3; the Q30 shape held, with these Scope C
consequences:
- Schedule source is the v4 `spec.schedules` array first, with the v3
  singular `spec.schedule` as fallback (mirroring the Scope A serde
  alias); `suspend` short-circuits reconcile, and an unparsable
  schedule surfaces as a `SubmissionError` condition rather than a
  crash-loop.
- The cron engine is a pure in-repo Vixie-cron subset (no external
  `cron` crate dependency): 5-field expressions with `*`/lists/ranges/
  steps, dom-vs-dow EITHER semantics when both are restricted,
  dow 7 == Sunday, the `@hourly/@daily/@midnight/@weekly/@monthly/
  @yearly/@annually` aliases, and `@every <duration>` with compound
  `s`/`m`/`h`/`d` pairs. Evaluation is minute-granularity and
  strictly-after (`next_after`), capped at a 366-day horizon. Timezone
  handling is explicitly out of scope: schedules are UTC-naive.
- concurrencyPolicy semantics: `Allow` (default) always fires;
  `Forbid` skips the due tick WITHOUT advancing `lastScheduledTime`
  (upstream parity — the missed fire is not silently consumed);
  `Replace` deletes the active children first, then fires.
- Each fired child is a generated-name Workflow
  (`<cwf-name>-<unix_ts>`) carrying `spec.workflowSpec` verbatim plus
  the `workflowMetadata.labels` overlay, the label
  `workflows.argoproj.io/cron-workflow`, and an ownerReference to the
  `CronWorkflow/argoproj.io/v1alpha1` parent — so child cleanup rides
  the Q20 garbage-collection cascade machinery, with no bespoke reaper.
- With no upstream on-disk scheduling anchor available, the first
  evaluation is eager (fires immediately if due); the anchor persists
  in `status.lastScheduledTime` thereafter, which is written only when
  a child is actually created.

---

## Q31 — logs/exec data plane: plain-HTTP single-shot relay (T4.2 Scope C)

- **Context.** Upstream `kubectl logs|exec` rides SPDY/WebSocket
  bidirectional streams between apiserver and kubelet :10250, with
  token authn + SubjectAccessReview authz on every hop. Reproducing the
  streaming subprotocol for a single-host distribution would buy
  complexity long before it buys capability.
- **Options.** (a) Full SPDY/WebSocket channel parity; (b) no exec/logs
  at all until Phase 3; (c) plain HTTP single-shot relay shaped like the
  upstream URL hierarchy.
- **Decision.** (c). The kubelet serves plain HTTP on
  `<node-ip>:<kubelet-port>` (default 127.0.0.1:10250; `--node-ip` /
  `--kubelet-port` flags): `GET|POST /pods/{pod_uid}/containers/
  {container}/log|exec`. `{container}` = `-` auto-selects the
  lexicographically smallest container name. `tailLines=N` is parsed as
  a raw integer. Errors are exact k8s `Status` JSON objects. Exec takes
  `{"command":[...]}` and answers 200 `{"exitCode":N,"output":"..."}` —
  a non-zero remote exit code is a NORMAL 200; only infrastructure
  failure (pod missing, node unreachable, crictl broken) yields 502.
  The apiserver relays `/api/v1/namespaces/{ns}/pods/{name}/log|exec`,
  resolving Pod→nodeName→Node→hop (404 for unknown pod/node, 502 for
  hop failure). stdout+stderr are MERGED in v1 (single CRI stream);
  `-f/--follow` is implemented CLIENT-side as a 250 ms poll of
  `tailLines`, with tail applied only to the first poll.
- **Consequences.** Two documented deviations from upstream: (1) no
  byte-split streams / attach / port-forward subprotocol in v1;
  (2) follow is poll-based, so log rotation under follow can repeat or
  skip lines across rotations. Trust model: single-host first — bind
  defaults to loopback; TLS + authN on the kubelet listener are
  explicitly deferred to the T1.3 security workstream and the Deferred
  list of T4.2, not silently assumed. Pod addressing keys on real
  `metadata.uid`, which is why Sprint 24 also fixed uid stamping at all
  three write seams (apiserver create, SSA create branch,
  StorageClient.create).

**S52 落地注记 (Q65 对齐).** T4.5 反向中继隧道落地时, Q31 单请求/响应
语义原样保持——中继只换传输（agent 长轮询反向回传）, 不引入流式子协议;
HTTP 错误状态不触发回退, 回退失败与直连失败同 502 形状, kubectl 侧零
感知。exec 流式仍不在 v1 范围（见 Q65 Consequences）。

---

## Q32 — Router rides openrusty: h2c gateway embedded as a library, Lua data plane deleted (T5.1–T5.4 / Sprint 25)

**Date.** 2026-08-28 (Sprint 25, "link_repos wave").

- **Context.** The built-in Router (Q4) was built as an openresty Rust
  port: an mlua/Lua data plane (coroutine↔async bridge Q12, per-phase
  coroutines + filter semantics Q13/Q14, `resty.*` + shared dicts Q15)
  over a hand-rolled HTTP/1.1 stack (raw TCP because axum's `Send` bound
  is incompatible with the `!Send` Lua VM). That bought programmability
  but kept a permanently bespoke HTTP substrate — the router spent its
  tests on the plumbing (phase chain, cosocket, resty stdlib) instead of
  the product. Sprint 25 adopted three upstream Rust repos as path
  dependencies (`link_repos/{openrusty,rdb,flannel-rust}` symlinks,
  git-ignored; root `Cargo.toml` `exclude = ["link_repos"]`), and wave 0
  upstream work made openrusty embeddable: lib targets
  (`openrusty-core` / `openrusty-server` usable as libraries),
  `state::from_config`, `reload::apply_config`, `RouteConfig.host`
  (203 upstream tests).
- **Options.** (a) Keep the mlua plane and keep porting openresty
  pieces piecemeal (Q15 roadmap: digests, full lua-resty-* corpus);
  (b) supervise openrusty as a child process (k3s-style bundling);
  (c) embed openrusty as an in-process library and delete the Lua plane.
- **Decision.** (c). `crates/router` now embeds the openrusty h2c
  gateway; the entire mlua data plane is deleted — the T5.1 bridge, the
  T5.2 phase pipeline (`ngx.*`, request-body hooks), the T5.3
  `resty.*`/shared-dict/cosocket stack, the T5.4 Lua routing + TLS +
  Lua hot-reload machinery, and `scripts/router-coroutine-selftest.sh`.
  The crate keeps the product-shaped parts: reflectors (services +
  endpoints, `networking.k8s.io/v1` Ingress), the `RouteTable` route
  model, and two gateways (per-`nodePort` + Ingress) behind the shared
  503-on-empty-Endpoints gate, served on ONE dedicated thread with a
  current-thread runtime. `networking.k8s.io/v1` Ingress + IngressClass
  become first-class in `crates/api` (schema-registry + discovery
  registration) and the source of truth for routing — the LuaRouter CRD
  is no longer the primary path. The NodePort ServiceLB plane (Q28) is
  ported onto the openrusty plane with its wire behavior intact
  (ST1–ST6); `NodePortConfig.ingress_listen: Option<SocketAddr>` (default
  `None`) exists for the Ingress listener but the CLI flag is not wired
  yet (debt). Golden: `discovery-apis.json` regenerated (networking
  group added), new `golden/discovery-networking-v1.json`, new check
  **G31** (inventory G01–G31, 39 PASS sites).
- **Consequences.** Lua programmability is gone — a deliberate scope
  cut: no `ngx.*`, no per-phase hooks, no `resty.*` library, no
  Lua-driven cert issuance; Q12–Q15 remain the historical record of the
  spike that proved the plane could exist. openrusty becomes a hard
  dependency: a fresh clone needs the `link_repos/openrusty` symlink in
  place to build (documented in the root `Cargo.toml` comment). Hot
  reload / live-config work (T5.5) continues on the openrusty
  `apply_config` seam instead of the Lua generation swap; shared-dict,
  drain and dynamic-cert equivalents must be re-shaped for openrusty.
  Golden discovery grew G31. Wire deviations from the Lua era (wildcard
  hosts, narrower TLS termination surface, no active health probes) are
  documented per-module in the router crate; Q28's polarity (503 on
  empty Endpoints, `--disable-kube-proxy` opt-out, no ClusterIP
  dataplane) is preserved unchanged.

---

## Q33 — Durable datastore is rdb:// (RocksDB via the rdb crate); sqlite:// is Unsupported (T2.3 / Sprint 25)

**Date.** 2026-08-28 (Sprint 25, "link_repos wave").

- **Context.** Q29 closed T2.3 with `SqliteStorage` on libsql core-only:
  a kine-style append-only `kv` table where the row table was both the
  live-object store and the revision log, plus explicit PRAGMA
  management and hand-rolled tx hardening. Sprint 25 adopted rdb — a
  RocksDB-backed versioned KV store with etcd-ish semantics — as a path
  dependency (`link_repos/rdb`), and wave 0 upstream work added the
  `store` feature gate (981 upstream tests). The engine now supplies
  natively what the SQLite layer re-implemented: versioned keys,
  lexicographic ordering as revision ordering, atomic multi-key writes,
  and column-family-grade prefix separation.
- **Options.** (a) Keep sqlite and add rdb as a third backend;
  (b) swap the durable slot to rdb and delete the sqlite implementation;
  (c) drop durability and keep only the in-memory embedded store.
- **Decision.** (b). New `crates/storage/src/rdb_backend/`
  (`mod.rs` + `keys.rs` + `watch.rs`): `RdbStorage` on the rdb crate via
  its `store` feature. Key layout inside one CF: `meta/revision` (the
  cluster revision, 8-byte big-endian u64), `meta/compact` (compaction
  watermark = lowest retained log rev), `obj/<key>` (live `StoredEntry`
  JSON; deletion = absence), `log/<rev big-endian>` (one `LogRecord` JSON
  per successful write). `log/` keys are fixed-width big-endian so
  lexicographic order IS revision order — a range scan is a
  revision-ordered replay. One write = one atomic WriteBatch
  (obj upsert/delete + log append + revision bump) with fsync. The etcd
  semantics (monotonic revision, create/mod revision, CAS, watch
  historical replay, compaction watermark) are replicated from the
  former sqlite write path; `RdbStorage::open(&Path)` is async. DSN:
  `rdb://<path>` selects it via `Dsn::Rdb(PathBuf)` in
  `crates/cli/src/runtime.rs` (startup log line contains
  "rdb datastore (rocksdb) — T2.3/Q33", which `scripts/durability-e2e.sh`
  greps); **`sqlite://` now returns `Unsupported` with a pointer to
  `rdb://`** — a breaking DSN change. Deleted:
  `crates/storage/src/sqlite/` and
  `crates/storage/tests/sqlite_storage.rs`; libsql and mlua are removed
  from the root and storage `Cargo.toml`s. Tests:
  `crates/storage/tests/rdb_storage.rs` = 26 contract + 4 durability
  (reopen / revision-monotonic / watch replay across restart /
  compaction watermark) + double-open exclusion.
- **Consequences.** Defaults are unchanged: `EmbeddedStorage` stays the
  default and the durable option remains an explicit opt-in (Q17). The
  kine-style append-only semantics survive the swap but on RocksDB, with
  `obj/` and `log/` separated (unlike SQLite, where the row table was
  both). One less FFI-ish dependency: libsql is out of the workspace
  entirely. The durability gate keeps its shape —
  `scripts/durability-e2e.sh` D1–D4 green on `rdb://`, including D3
  (identical resourceVersions across a SIGTERM/restart cycle) and D4
  (the revision counter resumes across restart, observed 15→16, plus
  watch replay). The 26 portable contract cases still run against BOTH
  backends via `storage_contract!`. Anything that scripted
  `--datastore-endpoint sqlite://<path>` must move to `rdb://<path>`;
  `scripts/local-up.sh` now boots
  `rdb://$DD/server/state.rdb`.

---

## Q34 — CNI is flannel-rust embedded in the agent process; node podCIDRs from `--cluster-cidr` (T4.3 Scope A / Sprint 25)

**Date.** 2026-08-28 (Sprint 25, "link_repos wave").

- **Context.** T4.3 was not-started: pods had podIPs from the CNI bridge,
  but node podCIDR allocation, pod-to-pod reachability over a real
  fabric, and the `NetworkUnavailable` condition were unowned, and Q28
  had explicitly reserved the pod-network CNI (and any ClusterIP
  dataplane) for this task. `link_repos/flannel-rust` supplies a flanneld
  daemon + flannel-cni plugin in Rust (350 upstream tests); wave 0
  upstream work added an `install_signal_handlers` option on flanneld
  `Options` so the daemon can be embedded without installing its own
  signal handlers — init-pro sets it to false and bridges SIGTERM
  through its own `infra::Shutdown` `CancellationToken`. The apiserver
  also lacked a node status write path for flannel's F2 (clearing the
  condition), and the vendored cni-plugins tarball had only loopback
  staged so far — no delegated CNI to put a pod on a real bridge.
- **Options.** (a) Supervise upstream Go flanneld as a child process
  (k3s bundling model); (b) embed flannel-rust in the agent process and
  ship the CNI plugin as a multicall alias; (c) defer CNI further.
- **Decision.** (b) for Scope A. `crates/cli/src/flannel.rs` runs the
  embedded flanneld (vxlan default backend) inside the agent process;
  the multicall alias `flannel` dispatches `flannel_cni::skel::run()` —
  the CNI meta-plugin invoked by containerd through the
  `<runtime>/aux/flannel` symlink that `runtime::stage` creates alongside
  `10-flannel.conflist` and the staged flannel binaries. Node podCIDRs
  come from a new controllers reconciler (`Kind::NodeCidr`) that
  allocates a /24 per node out of `ManagerConfig.cluster_cidr`
  (`--cluster-cidr`, default `10.42.0.0/16`). The apiserver gained
  `PUT/PATCH /api/v1/nodes/{name}/status` (`node_status.rs`) for
  flannel's F2. `crates/cli/src/flannel_keeper.rs` keeps
  `NetworkUnavailable=False` on the node in a 5s loop. C-side delegate
  fixes: `isDefaultGateway` dropped (it produced a second default route)
  and `iptables_forward_rules: true` pinned (the host's FORWARD-DROP
  default killed pod↔pod traffic). New CLI flags `--flannel-backend` and
  `--cluster-cidr`; new gate `scripts/flannel-e2e.sh` F1–F5.
- **Consequences.** Scope A covers single-node vxlan pod networking
  only: host-gw / wireguard backends, multi-node vxlan, and netpol are
  deferred, and upstream flannel conformance is out of scope for this
  slice (F5 records the gap; the T4.3 card tracks the remainder).
  `NetworkUnavailable` is maintained by a 5s keeper loop because the
  apiserver status patch has no merge key for `status/conditions` — a
  one-shot condition write is not yet possible (debt; an upstream-style
  status subresource merge is pending). ServiceLB L4 / LoadBalancer VIPs
  remain the T4.3 ↔ T5.6 coordination item, untouched by Scope A.
  Verified by `scripts/flannel-e2e.sh`: F1 podCIDR allocation,
  F2 flannel annotations + `NetworkUnavailable=False`, F3 podIP inside
  the node podCIDR, F4 pod↔pod over the vxlan fabric (F5 informational);
  the multicall selftest now counts 9 aliases including `flannel`.

## Q35 — Agent is an init-pro.io/v1 first-class resource scheduled natively; opencoder embedded as a library (T7.1 rewrite / Sprint 26)

**Date.** 2026-08-29 (Sprint 26, "native Agent").

- **Context.** T7.1 originally planned an `init-pro.io/v1alpha1 AgentWorkload`
  CRD whose controller materializes Pods, plus an out-of-process scheduler
  extender. With T3.2 (in-process scheduler + extender seam) and T7.3
  (Workflow first-class) landed, materializing Pods is the wrong level for AI
  sessions: a session is not a container — the binding (`nodeName`) is what
  matters, and Workflow `template.agent` should create the schedulable object
  directly. The opencoder engine (session/core/llm/store) exists upstream in
  Rust and can be consumed the same way as openrusty/rdb/flannel-rust.
- **Options.** (a) keep AgentWorkload→Pod materialization + external
  extender; (b) native `init-pro.io/v1 Agent` scheduled through the SAME
  filter/score/CAS loop as Pods, opencoder embedded as path-dep libraries;
  (c) external operator/crd outside the binary.
- **Decision.** (b) for Phase A. `crates/api/src/agent.rs` defines `Agent`
  (Namespaced, plural `agents`, shortName `ag`, list kind `AgentList`); spec
  carries the Pod scheduling-alignment fields (nodeName/nodeSelector/
  tolerations/affinity/schedulerName/resources, camelCase) plus the opencoder
  payload (agent/prompt/model/provider) — NO credentials ever enter a spec;
  keys resolve from env inside opencoder at runtime. The scheduler watches
  `init-pro.io/agents` with a dedicated queue and binds via kind-parameterized
  `bind_object`/`mark_unschedulable_object` (`bind_agent` writes spec.nodeName
  + a `Scheduled` condition; CAS + write-if-changed semantics unchanged);
  `Snapshot::build_with_agents` keeps agents informational — they consume no
  node capacity in Phase A, so Pod filter/score sums are byte-identical.
  Workflow `template.agent` (wins over `container` when both present) creates
  Agent children with the Workflow ownerReference; phase aggregation lists
  agents and maps `status.phase` like pod phases; GC sweeps agents via
  `DEPENDENT_KINDS`. opencoder rides `link_repos/opencoder` path deps
  (core/llm/store/session — `store` pulls libsql bundled-SQLite, an accepted
  build-cost class per the rdb precedent); `crates/agent` is the pure seam
  (AgentSpec→`opencoder_core::Config`, no I/O, no env reads). kubectl
  `resolve_target` maps agent/agents/ag → `init-pro.io/v1`/`agents`.
  Node-side execution (watch bound Agent → `opencoder_session::run_once` →
  status CAS) and a CLI run mode are Phase B (B1/B2; argv[0] `agent` is taken
  by the node agent, so B2 needs a new subcommand/alias).
- **Consequences.** T7.1 Phase A is done with zero apiserver handlers (the
  generic routes serve agents; discovery golden G04 regenerated, agents entry
  only). The scheduler is now multi-kind but Pod semantics are provably
  unchanged (G22/G23 + quiesce/fanout tests untouched and green). The original
  AgentWorkload→Pod semantics and "scheduler extender for agent placement"
  framing are retired; policy-grade placement (GPU/peer affinity) stays with
  T7.2. Phase B status writes go through merge-patch today (status-subresource
  merge is the same debt noted in Q34).

## Q36 — anyhome is an embedded first-class NFS memory service; Workflow `/workspace` auto-mount + Agent workspace annotation (T7.4 / Sprint 27)

**Date.** 2026-08-29 (Sprint 27, "anyhome + workspace").

- **Context.** Workflow/Agent workloads had no memory plane: a Workflow's
  steps shared nothing on disk, and an Agent (Q35) had nowhere to persist a
  session workspace, so "memory outlives the object that produced it" was
  unimplementable. `link_repos/any-home` (nfsserve) already provides an
  NFSv3 memory service with a `RealFs`/`MemoryDb`(libsql)/`Embedder`
  assembly, and it multiplexes NFS + MOUNT + portmap on ONE port, so it can
  be embedded without a portmapper/rpcbind dependency. Any consumer of that
  plane needs the same three answers — where the server exposes bytes, where
  containers mount them, and which object owns which subtree — or the
  server-side export paths and the pod-side mounts drift apart.
- **Options.** (a) out-of-process anyhome daemon + user-managed NFS PVs;
  (b) embed anyhome in `server` as a first-class plane, default-on, with a
  `--disable-anyhome` opt-out, and pin the path conventions in one shared
  module; (c) object-store/artifact sidecar instead of NFS.
- **Decision.** (b). `link_repos/any-home` is a path dep; `init-pro server`
  auto-starts it on `--anyhome-addr` (default `127.0.0.1:11111`, env
  `INIT_PRO_ANYHOME_ADDR` / `INIT_PRO_DISABLE_ANYHOME`) unless
  `--disable-anyhome`. Layout: `<data-dir>/anyhome/fs` (pre-created
  `workflow/` + `agent/` roots) + `<data-dir>/anyhome/anyhome.db`
  (libsql, `DummyEmbedder` at the upstream `--dim` default 64, validated on
  reopen), export `/`; graceful shutdown WAL-checkpoints the db. A server-side
  keeper (`crates/cli/src/anyhome_keeper.rs`) pre-creates one directory per
  Workflow/Agent by polling the store directly (Q19: no loopback HTTP, no
  token); deletion is deliberately ignored (A5: memory persistence beats GC).
  The path SSOT is `crates/common/src/workspace.rs`: `MOUNT_TARGET =
  "/workspace"`, `agent_workspace_path(id) = /agent/{id}`,
  `workflow_workspace_path(id) = /workflow/{id}` (ids guarded: no `/`),
  `workspace_nfs_url(addr, path)`. The Workflow controller injects, per step
  pod, a `workspace` NFS volume (`server = anyhome addr`,
  `path = /workflow/<wf-name>`) + a `mountPath: /workspace` volumeMount on
  the single main container (covers `container` and `script` template kinds)
  — skipped when the user already declared a `/workspace` mountPath or a
  volume literally named `workspace` (user declaration wins) and absent when
  anyhome is disabled. Agent children (Q35) carry the contract annotation
  `init-pro.io/workspace = nfs://<addr>/agent/<agent-name>`; the kubelet
  consumes the same shape through its new `nfs` volume kind (k8s
  `NFSVolumeSource`; `mount.nfs -o vers=3,tcp,nolock,noacl,port=P,mountport=P`
  — the same port twice because anyhome is single-port — idempotent via
  `/proc/self/mounts`, umount-before-rm on cleanup).
- **Consequences.** anyhome is a workspace plane, not a general-purpose
  storage product: `DummyEmbedder` means `/search` is exact-match only, and
  the plane degrades to a WARN (not a fatal) if it fails to start.
  **D1 constraint:** the path SSOT lives in `common`, NOT in `agent` —
  `agent` transitively pulls opencoder's libsql 0.9.30 while anyhome vendors
  libsql 0.10.0-pre.4, and both copies in one binary collide at link time
  (`duplicate symbol: update_hook_cb`); therefore no crate on the server
  binary's dependency path may depend on `agent`, which in turn keeps Agent
  EXECUTION Phase B (Q35 unchanged: B1 runs on nodes, outside this link
  graph). The Phase B/kubectl contract is the annotation string, so Phase B
  work cannot move the paths without touching `common`. Deletion/GC of
  workspace directories is out of scope (A5, needs an owner/subresource
  model upstream). Verified by `scripts/anyhome-e2e.sh`: A1 root-mount
  roundtrip, P2 subpath mount (`host:/workflow/<name>` WORKS — coordination
  item A2 resolved, no upstream change needed), K1 keeper directory creation
  after a Workflow POST.

**Superseded in implementation (Sprint 54 / Q66, 2026-11-25) — the anyhome
entity is decommissioned, the workspace-NFS contract stands.** The plane
this decision created is unchanged from the CONSUMER's point of view —
flags (`--anyhome-addr`, `--disable-anyhome`), envs, the annotation
strings, the volume names, the on-disk `<data-dir>/anyhome/fs` layout,
the keeper, the path SSOT in `common::workspace`, and the single-port
NFS/MOUNT/portmap posture all survive verbatim (Q43's injection chain
rides it untouched). What dies is the implementation base: the any-home
crate assembly (`RealFs` + `MemoryDb` + `DummyEmbedder`), the
`anyhome.db` file, the EMBED_DIM drift guard, and the vendored libsql
plane — none of the `MemoryDb` retrieval semantics (the `/search` RPC,
the exact-match `DummyEmbedder`) ever had a consumer in init-pro, so
nothing observable is lost. The plane is now served by an in-tree
directory VFS (DirFs, adapted from nfsserve's mirrorfs example,
BSD-3-Clause attribution in NOTICE) mounted directly on `NFSTcpListener`
— see Q66. D1 below loses its original rationale (the vendored
libsql 0.10.0-pre.4 that collided with opencoder's crates.io 0.9.30 copy
no longer exists in the binary's link graph); the constraint itself is
RETAINED as link-graph hygiene — the server binary stays free of any
`agent`/opencoder linkage, and the path SSOT stays in `common`.

## Q37 — T7.1 Phase B: node-side Agent execution (opencoder CLI subprocess) (2026-08-31)

**Context** — Q35 shipped the `Agent` resource end-to-end (API, scheduler
binding, Workflow `template.agent` subobjects) but nothing EXECUTED a bound
Agent on its node. Phase B needs a node-side loop that watches bound Agents,
runs the opencoder payload, and writes phase/conditions back — over the
anyhome workspace plane (Q36) shared with the server-side keeper.

**Options** — (a) execute inside the kubelet pod loop by materializing Pods
(rejected: Q35 deleted Pod materialization); (b) a separate daemon binary
(rejected: Q1 single multicall binary, `agent` subcommand already owns node
supervision); (c) a watch+execute loop embedded in the `agent` subcommand,
one opencoder CLI subprocess per Agent (chosen).

**Decision** — (c). `kubelet::agents` runs LIST+watch over
`/apis/init-pro.io/v1/agents` with a client-side `spec.nodeName` binding
filter (same filter on LIST and watch events), drives every Pending bound
Agent through one attempt: resolve runner -> mount/prepare workspace ->
spawn opencoder as a subprocess (args via argv, credentials ONLY via child
env, Q35) -> merge-patch status write-if-changed to
`/apis/init-pro.io/v1/namespaces/{ns}/agents/{name}`. State machine:
Pending -> Running (`Executing`) -> Succeeded (`Completed`, `exit 0; session
<id>`) / Failed (`RunnerFailed` exit code, `ExecTimeout` on the exec budget,
`WorkspaceMountFailed`/`WorkspacePrepareFailed`, `RunnerMissing` when no
runner binary is discoverable). A DELETE observed in LIST or watch kills the
subprocess via `/proc/<pid>`. Runner discovery order: env
`INIT_PRO_AGENT_RUNNER_BIN` (set-but-missing is terminal, no fallback) ->
staged `<data>/bin/opencoder` -> embed `<data>/data/current/bin/opencoder`
-> link_repos ancestors of the exe/cwd (dev layouts). Workspace source:
`init-pro.io/workspace` annotation `nfs://<addr>/agent/<name>` -> else
`--anyhome-addr` -> else node-local `<data>/agent-ws/<name>` (empty addr =
loop on, node-local fallback; only `--disable-agent-exec` turns the loop
off). The keeper (Q36) scaffolds the SHARED bytes (skills/tools/mem +
prompt template); the node creates its per-Agent leaves THROUGH the mount,
so they are visible to other nodes.

**Coordination notes (upstream-frozen behaviors, verified empirically)** —
1. any-home's mountd resolves export paths against the tree as of server
   boot; out-of-band disk writes after boot are invisible to path
   resolution. Therefore the node mounts the PARENT export (`/agent`, boot
   present) once per server address into
   `<data>/agent-ws/nfs-<addr>` (persistent across Agents, idempotent via
   /proc/mounts) and works in `<leaf>` through it. 2. `mount.nfs` parses
   `host:port:/path` as hostname + dirpath `port:/path` and sends that
   dirpath verbatim (observed on the wire: `43141:/agent`), so the mountd
   resolves the junk component and answers MNT3ERR_NOENT. The mount SOURCE
   must be `host:/path`; ports travel only via `port=`/`mountport=`
   options. 3. Golden agent boots (G24-G30, nscascade, workloads) pin
   `--disable-agent-exec`: the loop must stay hermetic against
   link_repos runner discovery (a dev-built opencoder binary must not flip
   golden cases from RunnerMissing to mounting/executing).

**Consequences** — Agent execution needs only the API surface (runs even
when the CRI stack is off); opencoder stays a subprocess (D1), so its
libsql linkage never enters the init-pro binary (Q36 D1 unchanged). The
state machine is merge-patch based (status subresource merge semantics
remain a Q34-style debt). Gate: `scripts/agent-exec-e2e.sh` S1/S2+G1-G7
(NFS mode), kubelet `agents_runner_integration` (6 tests, local fallback +
binding-filter regression), `crates/runtime/src/opencoder.rs` unit tests.

---

## Q38 — link_repos pin-clone bootstrap + intranet h2c release posture (2026-09-05)

**Context** — Release was blocked by two P0s. (1) Cold clones could not
build: `link_repos/` (7 path-dep repos: openrusty, rdb, flannel-rust,
opencoder, any-home, plus nfsserve/libsql, then both nested inside the
any-home pin — nfsserve moved top-level 2026-11-15, see Decision) is
gitignored
and materialized only via local `/root/*` checkouts + symlinks — zero-cache
checkouts (CI, Docker, new machines) failed at path-dep resolution, and
local green relied on manually pinned /tmp snapshots because the openrusty
live tree carries uncommitted TLS-refactor WIP (new `dynamic` /
`UpstreamConfig.tls` fields break k3as's exhaustive config construction).
(2) Shared deps were un-pinned: rdb's validated line was rewritten out of
its own upstream main, and the release needed a network posture call on the
Router plane.

**Options** — for provisioning: (a) git submodules (rejected: nested
any-home layout + patch handling + the Q35/Q36 link_repos convention);
(b) vendor the sources into this repo (rejected: ~7 foreign histories
inlined, license noise, no upstream movement); (c) checked-in pin manifest
+ clone/checkout bootstrap script (chosen). For release posture: wait for
openrusty TLS vs ship intranet h2c on the pinned `4180ad3` — chosen: ship
h2c now, pin `4180ad3`, treat TLS as post-release forward adaptation.

**Decision** — (c) + h2c now. `link_repos.lock` (TOML: name/url/sha +
path/patch/marker/also_link) is the SSOT for pins;
`scripts/bootstrap-link-repos.sh`
materializes it: clone+`checkout --detach <sha>` when missing, verify-only
for existing/symlinked dev checkouts (HEAD==sha; tracked-dirty refusal with
`--force` escape; untracked artifacts ignored). nfsserve's AUTH_SYS patch
(Q37) is applied on top; the canonical copy is versioned in THIS repo at
`scripts/patches/nfsserve-auth-identity.patch` (amended 2026-11-15: the
patch originally rode INSIDE the any-home pin; when nfsserve was re-pinned
top-level the copy moved to the consumer repo, so patch and consumer
advance commit-by-commit instead of the patch moving with any-home).
nfsserve materializes TOP-LEVEL at the default path `link_repos/nfsserve`;
any-home's own workspace resolves nfsserve at `link_repos/nfsserve`
RELATIVE TO THE ANY-HOME ROOT, so the lock's `also_link` keeps
`link_repos/any-home/link_repos/nfsserve` a relative symlink to the
top-level checkout. Cargo keys path deps by LEXICAL path and never
canonicalizes symlinks, so the root `Cargo.toml` deliberately references
that same nested path — one lexical identity, ONE nfsserve lockfile
package (a top-level path there forks the graph into two same-named
packages and the build dies with a lockfile collision). Bootstrap
migrates a legacy REAL dir at an also_link path to the symlink only when
it provably reproduces from the pin (git repo at the pinned sha whose
only dirt is the patch marker); dirty or non-git trees die, `--force`
downgrades. Both CI jobs and the Dockerfile builder run the
bootstrap before compiling. rdb's pin is published as branch `pin/init-pro`
on MoSunDay/rdb (upstream main is a parallel rewrite, d52ef49). The Router
ships intranet h2c at openrusty `4180ad3`; when the TLS work lands, bump
the pin and forward-adapt `gateway_config.rs` (`dynamic: None` handling,
`tls: None` at the `UpstreamConfig` construction, the 00a0483 pattern).

**Consequences** — every push CI-proves cold-clone buildability; manual
pinned-snapshot verification is retired. Local dev: symlinks stay valid
(verify-only), but building k3as against WIP link_repos trees requires
`--force` + awareness that uncommitted upstream API drift breaks k3s-side
exhaustive construction. Pin bumps are single-lockfile diffs (revert =
single commit). Non-build reference checkouts (argo-workflows, flannel)
stay unmanaged and are documented as such. Discovered during closure: the
transitive anyhome->libsql path dep makes libsql a 7th mandatory pin (the
6-repo assumption was wrong).

**Revised (Sprint 54 / Q66, 2026-11-25) — mandatory pins 7 → 6, the
also_link doctrine retires.** any-home and its nested libsql pin are
REMOVED from `link_repos.lock` (the anyhome entity is dead, Q66);
nfsserve loses its `also_link` entry because no consumer resolves
nfsserve through any-home's internal path anymore — the root
`Cargo.toml` now points at the TOP-LEVEL `link_repos/nfsserve`, and with
it the "one lexical identity" nested-pin doctrine (lexical-path keying,
the `link_repos/any-home/link_repos/nfsserve` symlink, the
any-home-precedes-libsql ordering rule) is deleted from the manifest
doctrine. The AUTH_SYS caller-identity patch stays exactly as is
(canonical copy in this repo, marker `src/identity.rs`, Q37). The
any-home reachability note (the 42b8d8e re-pin after the upstream
history rewrite) is now historical. Cold-clone posture is otherwise
unchanged: 6 mandatory pins — openrusty, rdb, flannel-rust, opencoder,
nfsserve, arust-workflow — plus the reference-only checkouts
(argo-workflows, flannel, kiri) documented above.

---

## Q39 — Pod log layout: date directories + 100 MiB copytruncate rotation (T4.6) (2026-11-06)

**Context** — The deployment profile behind init-pro wants pod logs
organized "按日期目录，单文件 ≤100MB" (per-day directories, no live log
file above 100 MiB). As-built pre-T4.6, log files lived flat at
`<data_dir>/pods/<pod-uid>/<container>.<attempt>.log` (T4.2 Scope C,
Q31) with zero rotation anywhere: containerd's config template carries no
log caps, and Q31 explicitly treated rotation as an out-of-scope
deviation. The files are written and fd-held by the vendored containerd
via the CRI `log_directory`/`log_path` pair, so whatever rotates them
must not delete/rename the live inode out from under the writer.

**Options** — (a) rely on containerd's built-in `max_container_log_size`
(rejected: the vendored config has none, k3s itself does not set it, and
it would rotate inside a foreign tree we pin rather than own); (b) rename
rotation a la logrotate without copy (rejected: the writer's open fd
follows the inode, so post-rotation output would vanish into the renamed
file); (c) external copytruncate rotation in the kubelet, the same shape
as upstream kubelet's `containerLogManager` (chosen); layout either
top-level `logs/<date>/<pod>/` or per-pod `pods/<uid>/<date>/`.

**Decision** — (c) with the per-pod layout: sandbox `log_directory`
becomes `<data_dir>/pods/<pod-uid>/<YYYY-MM-DD>/` (UTC date pinned at
plan time, injected into the pure `plan()`; `log_path` stays relative so
containerd joins the pair itself), and a 5th kubelet task sweeps every
60s: any live `*.log` >= 100 MiB (`LOG_MAX_SIZE`) is copytruncated —
copy -> `.1`, shift `.N-1` -> `.N`, evict beyond `LOG_MAX_FILES`=5
generations, truncate the live file in place. Only date-shaped
subdirectories are ever considered, so the `volumes/` sibling tree is
structurally immune. Read side resolves the highest attempt across every
date dir (newest date as tie-break) so a container alive across midnight
stays readable, with the pre-T4.6 flat layout as a fallback. 100 MiB
(vs upstream kubelet's 10 MiB default) is the deliberate deployment
knob; 5 generations bounds worst-case per-container disk at ~600 MiB.

**Consequences** — Q31's external-rotation deviation now has a concrete,
owned implementation instead of a documented gap. A truncation window
inherently loses any bytes written between copy and truncate (same
trade-off as every copytruncate deployment; a 60s scan bounds
worst-case overshoot to one scan period). Logs no longer rotate on
container restart alone (upstream rotates at each container start); the
date directory provides the per-day segmentation instead. `kubectl logs
-f` needs no change: its existing file-shrink detection already treats
truncate as "rewind". Golden conformance (T0.6) is untouched — the wire
surfaces (`pods/log` relay content) are byte-identical. Future knob
surfacing (CLI flags for size/generations) is a plain
`LogRotatePolicy` plumbing change when someone asks for it.

---

## Q40 — Embedded Docker Registry HTTP API v2 image plane + newest-N tag retention (T6.3) (2026-11-06)

**Context** — The deployment profile behind init-pro wants a usable image
registry "built into k3as, no external install": cluster nodes (and
operators' docker/containerd/crictl clients) must be able to push and
pull images from the server itself, offline, without running a separate
registry (distribution), docker daemon, or cloud endpoint. The profile
additionally demands tag management: per repository keep only the newest
N versions (default 10) while explicitly pinned tags are kept forever and
must not consume the retention budget. The repo already hosts embedded
first-class planes following the same doctrine — anyhome NFS (Q36),
flannel CNI (Q34), Router (Q32) — all default-on inside `init-pro server`
with `--disable-*` opt-outs.

**Options** — (a) re-export upstream distribution as an external process
(rejected: an extra daemon + TLS/auth config to babysit, contradicts the
"one multicall binary, no external install" rule, and upstream is Go);
(b) a filesystem-only "registry-like" store without the V2 protocol
(rejected: docker/containerd/crictl would not speak it — clients must
work unmodified); (c) implement the Docker Registry HTTP API v2 subset
directly in-process on axum behind a content-addressed store, with the
retention/keep-forever policy as pure functions run on every push
(chosen).

**Decision** — (c), shipped as new crate `crates/registry` (T6.3) wired
into `init-pro server` exactly like anyhome: default-on at
`--registry-addr` = `127.0.0.1:15000` (port 5000 is the docker
convention but collides with macOS AirPlay; v1 assumes bind == advertise,
so multi-node setups pass `--registry-addr 0.0.0.0:<port>`, and a second
server on the same host must pick a distinct port), opt-out
`--disable-registry`, retention knob `--registry-tag-retention` (default
10; 0 = unlimited), keep-forever patterns `--registry-keep-tag` unioned
with a hot-reloaded `<data-dir>/registry/keep.json` (`repo:tag` with `*`
wildcards; a bare `repo` pins the whole repository; `*` alone is
rejected as a footgun). Wire surface: ping, `_catalog`, `tags/list`,
manifest GET/HEAD/PUT/DELETE, blob GET/HEAD/DELETE, resumable uploads
(POST/PATCH/PUT/DELETE), monolithic single-POST pushes, cross-repo
mount; `Docker-Content-Digest` on manifest/blob GET/HEAD/PUT responses;
sha256 digests only; docker-v2 + OCI manifest/list/index media types.
Blob GET streams the CAS file with an explicit `Content-Length` (layer
pulls never buffer the whole blob in memory); HEAD is metadata-only, and
Range/Accept-Ranges remains a follow-up. Storage is
`<data-dir>/registry/` — a GLOBAL sha256 blob CAS
(`blobs/sha256-<hex>`) plus per-repo manifests by digest and per-tag
JSON records `{digest, mediaType, pushed_at (millis), seq, size}`.
Retention runs synchronously on every manifest push: count non-pinned
tags, keep the newest `retain` by `(pushed_at, seq)` (tie-break reverse
tag name for determinism), delete the rest, drop manifests that lost
their last referencing tag, then sweep blobs referenced by no stored
manifest with a 600 s freshness grace window (a blob upload and its
manifest PUT are two client round-trips; the window keeps a concurrent
push from being collected mid-flight). Deleting by tag is refused
(`DIGEST_INVALID`, spec-aligned: clients delete by digest); deleting by
digest removes the manifest + every tag pointing at it and reclaims
unreferenced blobs. Deleting a digest that matches no stored manifest and
no tag answers 404 `MANIFEST_UNKNOWN` (202 only when something was
removed). Referential validation on push: config/layer/index-child
digests must be well-formed sha256 first (400 `MANIFEST_INVALID`
otherwise, checked before any filesystem probe) and then exist in the
CAS/repo (`MANIFEST_BLOB_UNKNOWN` otherwise). The registry's graceful
drain is bounded (10 s); on timeout the listener is aborted with a warn
so a stalled upload cannot wedge SIGTERM. No auth in v1 — the listener
defaults to loopback, matching the intranet h2c posture of the rest of
the control plane (Q38).

**Consequences** — Images now flow fully offline: `scripts/registry-e2e.sh`
pushes a fabricated docker-v2 image with curl and asserts retention /
keep-forever / delete semantics against a real booted server
(R1–R6; R7 = real `crictl pull` is env-gated and SKIPs cleanly by
default because `registry.k8s.io` is unreachable in CI — in Sprint 34 it
passed against a live containerd: `crictl pull` fetched the fabricated
image, 8 passed / 0 failed / 0 skipped).
Kubelet/containerd need no changes to pull from it — it is a plain V2
endpoint (agent-side mirror config is a follow-up, not required for the
node-local default). The global CAS means same-content blobs are stored
once across repositories and cross-repo mounts are metadata-free; the
trade-off is that `DELETE blob` removes the physical file repo-wide —
acceptable for a single-tenant embedded plane, and blob GC still gates
on manifest references. Blob GC scans every stored manifest per push,
which is O(repo manifest count) at v1 scale; a manifest-count trigger or
background cadence is the obvious next step if repositories grow large.
Tag retention is deliberately synchronous and failure-tolerant (a GC
miss never fails the client push — it only delays reclamation). Golden
conformance (T0.6) is untouched: the registry is a separate listener and
does not alter any apiserver wire surface. Known v1 limits: multi-arch
index children are pushed untagged and never age out (permanent
retention); `_catalog` is unpaginated; and `DELETE blob` removes the
global CAS file repo-wide (the trade-off noted above).

## Q41 — ServiceLB announce controller: embedded klipper-lb equivalent for LoadBalancer Services (T4.3 Scope B / T5.6) (2026-11-09)

**Context.** `type=LoadBalancer` Services already got nodePorts allocated
by the apiserver, but nothing announced `status.loadBalancer.ingress` —
the Service plane (T5.6) had no address to route to and no k3s-parity LB
story. k3s ships svclb/klipper-lb as a DaemonSet; init-pro embeds the
equivalent instead (single-binary doctrine, Q1).

**Options.** (a) **embedded announce controller + nodePort data plane**
— a controller that CAS-writes `status.loadBalancer.ingress = [{ip}]`
while the nodePort listeners (already served through the Router/openrusty
plane, Q28/Q32) act as the L4 backend. (b) a svclb Traefik-style DaemonSet
addon — rejected: external addon deployment contradicts the
first-class-embedded posture. (c) external MetalLB only — rejected as the
default (no CRDs/addon churn), though it must remain POSSIBLE.

**Decision.** `crates/controllers/src/controllers/service_lb.rs` (Q41,
commit a9a61db): the embedded announce controller. VIP selection is
k3s/klipper-lb semantics — explicit `spec.loadBalancerIP` wins, else the
first Node InternalIP is announced (the nodePort plane listens on every
node, so any node IP reaches the Service). Ownership discipline: a
Service that already carries a NON-EMPTY `status.loadBalancer.ingress`
is never rewritten — an external MetalLB/cloud provider may own it
(MetalLB stays possible; `--disable servicelb` removes the controller
from `ManagerConfig` so an external LB manager can take over, wired via
`ServerBind.disable_servicelb`). Pure `desired_ingress()` decision fn;
the reconciler reads the Service AND Node list fresh through the Client,
then CAS-writes the status (lost CAS → `Conflict` → rate-limited requeue,
same shape as `node_cidr::allocate`). The Router's Service plane consumes
the same field: `router/src/endpoints.rs::parse_service` reads
`status.loadBalancer.ingress[0].ip` (`ingress_ip`) as the upstream
address for LB Services (empty-string announcements filtered to `None`).

**Consequences.** (+) LoadBalancer Services become usable end-to-end
(announce → Router upstream → nodePort traffic) with zero addons.
(+) `--disable servicelb` hands the plane to any external LB manager
without code changes. (−) L4 forwarding itself still rides the
nodePort/Router plane — klipper-lb's per-Service host-port binding is
NOT reimplemented (T5.6 scope line: "NodePort plane done, openrusty
承载"); direct host-port binding remains future work.

## Q42 — workflow engine + model extracted to the standalone arust-workflow repo (T7.3, 8th link_repos pin) (2026-11-10)

**Context.** The link_repos wave (Q38) had already absorbed every other
heavy component as a pinned external Rust repo — openrusty (Q32),
rdb (Q33), flannel-rust (Q34), opencoder (Q35), any-home/nfsserve (Q36).
Only the Workflow plane still lived inside k3as: the argoproj.io/v1alpha1
typed port (`crates/api/src/argoproj/`, 17 modules) plus the engine
(`crates/controllers/src/controllers/{workflow_plan,dag,phase,status,pod,
agent,cron_expr}.rs`). That couples model and engine iteration to the
distro repo and blocks independent reuse/testing.

**Options.** (a) keep in-tree — rejected: the last un-extracted heavy
component, contradicting the wave's direction. (b) one flat crate with
model+engine — rejected: model is pure data/serde (no deps beyond
k8s_openapi/serde) while the engine needs tokio/time plumbing; consumers
(api crate) want the model WITHOUT the engine's weight. (c) **two-crate
workspace `arust-workflow`** — `workflow-model` (types) + `workflow-engine`
(plan/dag/phase/status/pod/agent/cron_expr + workflow & cronworkflow
reconcilers as pure functions over a small `Kube` seam trait), consumed by
k3as through thin adapters.

**Decision.** Extract to `github.com/MoSunDay/arust-workflow`
@ `687ed3257b47240ac3564ebbef0b6181490708b3` — the **8th** link_repos pin
(Q38 bootstrap, SHA-verified; origin/main tip like the other MoSunDay
pins). Repo shape: `#![forbid(unsafe_code)]`, 79 unit tests (engine 49 +
model 30), clippy `-D warnings` clean, biggest file 550 lines (pod.rs).
k3as side: `crates/api/src/argoproj.rs` is now a one-file re-export shim
(`pub use workflow_model::*;` + the kept `register()`/`GROUP`/`VERSION`)
so the api crate's public surface and the `argoproj_fidelity.rs`
fixtures are byte-identical; `crates/controllers/src/controllers/
workflow_bridge.rs` (NEW) implements the engine's `Kube` trait over the
existing HTTP `Client` with bidirectional error mapping
(`Conflict`/`AlreadyExists`/`NotFound` ↔ engine variants) plus
contract-drift tests pinning the workspace/time seams; `workflow.rs` /
`cronworkflow.rs` are thin adapters (`EngineClient(client)` +
`from_engine`), and `runner.rs`/informer wiring is untouched. Workspace
deps: `workflow-model`/`workflow-engine` path deps in the root
`Cargo.toml` (same pattern as openrusty/rdb).

**Consequences.** (+) The engine and the argoproj model iterate in their
own repo; k3as diff surface shrinks ~6.7k lines for this wave.
(+) Wire stays JSON-only (Q10) and controller behavior is bit-identical
(bridge drift tests + the full existing controllers suite).
(−) Schema registration stays k3s-side via the shim, so model renames
still need a k3as touch; engine changes need a pin bump (rdb `pin/*`
precedent, Q38).
(−) The argoproj fidelity corpus lives on both sides (fixtures in k3as
`tests/fixtures/argoproj/` exercise the model through the shim) — the
model crate's own 30 tests are the upstream-truth mirror.

## Q43 — anyhome Pod 级注解挂载：apiserver defaulting 注入 nfs 卷 (T7.5) (2026-11-09)

**Context.** T7.4 (Q36) made anyhome a first-class embedded NFSv3 memory
plane, but the workspace contract only existed for two controller-owned
workload shapes: Workflow step pods (the controller hard-injects a rw
`/workspace` nfs volume) and Agents (the `init-pro.io/workspace`
annotation). An arbitrary Pod could not opt into the plane at all — no
per-pod workspace, no ro/rw choice, no custom shared paths. The kubelet
already supported the generic k8s `nfs` volume type including
volume-level `readOnly` (Sprint 20, T-anyhome), and the e2e P2 probe had
proven subpath mounts against the pinned single-port nfsserve — so the
feature needed zero pin changes and zero kubelet changes. Q-code
registry note: Q41 (ServiceLB, commit a9a61db) and Q42 (workflow-engine
extraction, `workflow_bridge.rs`) now have their own decisions.md entries
(recorded 2026-11-10, after Q43).

**Options.** (a) **apiserver defaulting at create + server-side-apply
time** — the same pattern as Service `service::prepare` (Sprint 18/S3):
when the annotation is present and anyhome is on, inject the nfs volumes
+ mounts into the body before persistence; `--disable-anyhome` leaves
everything inert. (b) **controllers-side reconciler patch** — a
controller patches pods after creation; rejected as the primary path
because the container may already be starting (the kubelet could see the
pre-patch spec and skip the mount — a race by construction). (c)
**kubelet-side synthesis** — the kubelet reads the annotations and
synthesizes host mounts itself; rejected because it needs
`KubeletConfig` to learn the anyhome address and `PodView` to grow
annotations — a wider seam change, and the injected volumes would never
appear in the stored spec (invisible to kubectl/diff tooling).
(d) Server-side per-path read-only enforcement via nfsserve vfs
capabilities; rejected — requires a pin patch for a v1 feature, and
native k8s NFS `readOnly` is client-enforced anyway.

**Decision.** Option (a), with these locked specifics:
- **Annotation schema** (SSOT in `common::workspace`): `init-pro.io/anyhome`
  = `rw|ro|true|false` (`true`≡`rw`, `false`=opt-out; unknown value →
  422 at create); `init-pro.io/anyhome-mounts` =
  `<server-path>:<container-path>[:ro|:rw],...` — both halves must be
  absolute paths with no `.`, `..` or empty segments; dedup by container
  path, first wins; every triple becomes its own nfs volume
  (`anyhome-<sanitized container path>`, deterministic RFC1123 ≤63) +
  volumeMount into every container lacking that mountPath.
- **Server layout** `/pod/<namespace>/<pod-name>`: the extra `pod/`
  segment is load-bearing — a namespace may legally be named `workflow`
  (or `agent`), so the literal `/<ns>/<name>` layout could collide with
  the existing `/workflow/<id>` and `/agent/<id>` roots.
- **Injection runs on EVERY server-side apply, not just the created
  branch**: SSA deletes fields a field-manager owns but omits from its
  new config, so created-branch-only injection would silently strip the
  volumes on that manager's next apply; pre-merging the injection keeps
  exactly one owner-consistent copy (name/path/mountPath dedup keeps it
  idempotent — repeated applies and `prepare(prepare(x)) == prepare(x)`).
- **A4 user priority** (same rule as the Workflow engine's
  `user_declares_workspace`): a container-declared volumeMount at the
  same mountPath, or a pod volume with the same name / same nfs path,
  suppresses injection — which also makes a Workflow step pod carrying
  both the engine's `workspace` volume and a user annotation collide
  safely instead of double-mounting.
- **`ro` is client-enforced** (`readOnly: true` on the volume → CRI
  readonly bind, OR-ed with the mount-level flag by the kubelet) — the
  same trust model as native k8s NFS readOnly.
- **Keeper extension**: the anyhome workspace keeper's 5s beat grows a
  third page (LIST pods, filter by annotation, `mkdir -p` the pod
  workspace + every custom server path); deletion stays deliberately
  ignored (A5 continuity — memory persistence beats GC; an owner model
  is the prerequisite for any reclamation).
- **Inert when disabled**: `--disable-anyhome` (or any non-server
  embedding) passes `workspace_nfs: None` — no validation, no byte
  changes; un-annotated Pods are byte-identical in every mode, so golden
  conformance (T0.6) is untouched.

**Consequences.** Any Pod can now carry durable anyhome memory (its own
`/workspace` plus arbitrary shared paths) with a one-line annotation,
and the injected volumes are visible, inspectable and diffable in the
stored spec (unlike kubelet-side synthesis). Known costs, accepted and
recorded: per-pod directories accumulate with no GC (A5 — high-churn pod
names leave directories behind until an owner model exists); the keeper
polls a full pod LIST per beat (fine at v1 cluster sizes, watch is the
future optimization); read-only is advisory against a malicious client
(the server trusts the mount flags, exactly like upstream NFS); and the
apiserver now owns one more admission-time defaulting path, so any new
Pod-level defaulting should follow the same `prepare` shape rather than
growing ad-hoc mutation sites. Golden conformance and flag parity are
unaffected (no new flags; P3-P5 joined `scripts/anyhome-e2e.sh` with the
existing curl/python3 degrade doctrine).

**Post-review hardening (2026-11-10, GO WITH CONDITIONS closed).** A
static review of the first cut surfaced two P1 defects and two P2 gaps,
all landed the same day:

- **T7.5a — derived-name collision disambiguation.** Two container paths
  that sanitize identically (`/A_B` and `/a-b` both → `anyhome-a-b`)
  used to make the second volume silently vanish while its mount still
  bound to the FIRST volume's server path (wrong content, wrong
  readOnly). `pod_workspace::resolve_volume_name` now resolves, in
  order: same-nfs-path alias (reuse the existing volume's name — this
  also fixes mounting one server path at several container paths, and
  binds the Workflow engine's `workspace` volume instead of dangling on
  an uninjected name); unclaimed base name; A4-owned base name (a
  user-declared volume with the derived name still WINS — bind, never
  rename); and only then a deterministic `-2`/`-3` suffix in annotation
  order. `ensure_container_mount` dedups per-mountPath (not per-name),
  which the alias case requires. Ownership is distinguished by
  snapshotting the incoming volume names BEFORE any injection: a name we
  injected earlier in the same call is a collision; a name the body
  arrived with is A4.
- **T7.5b — PUT replace joins the defaulting.** `item::do_replace`
  previously bypassed `prepare`, so `kubectl replace` from a stale
  manifest silently stripped the injected volumes. Replace now runs the
  same prepare hook as create/apply (idempotent by construction); the
  create/apply/replace write triple is uniform.
- **P2 — identity guard scoped to annotated Pods.** The
  namespace/name guard now fires only when an anyhome annotation is
  present; an un-annotated Pod with a `/`-bearing name behaves exactly
  as before the feature (no new 422 class on the golden surface).
- **Trust-model clarifications (no code change, by design):**
  `initContainers` are deliberately NOT injected (matching the Workflow
  engine's step-pod shape — init containers are setup, not workspace
  consumers; a user can still declare the mount manually), and any Pod
  can rw-mount any server path the annotation names — there is no
  per-path authz in v1, the same client-trust model as native k8s NFS;
  per-path ACLs would be a follow-up decision.

**Rebase note (Sprint 54 / Q66, 2026-11-25).** T7.5's semantics are
UNTOUCHED by the anyhome entity decommission (Q66): the annotation →
volume defaulting chain, the collision disambiguation ladder, and the
keeper pre-creation all ride the same single-port NFSv3 plane — only the
plane's engine under the hood changed (any-home's AnyHomeFs → the
in-tree DirFs). The decision above reads "anyhome" throughout as the
plane's historical name; every user-facing surface it specifies (flags,
annotations, volume names, server-path semantics) is byte-identical, and
the pinning tests (`crates/apiserver/tests/anyhome_pod_mounts.rs`, e2e
P3-P5) required zero changes.

## Q44 — T4.3 Scope C: ClusterIP 分配面 + Router VIP 承载面 (T4.3 Scope C / S51) (LOCKED 2026-11-22, S51 定稿即交付)

**Context.** Q28 locked the Service plane as NodePort-only in the Router
with the ClusterIP dataplane explicitly deferred. The S38–S48 semantic-
backfill iteration originally scheduled it for S42; per the phase-4-
closure 排程 it actually executed in S51 (网络链 I): without a ClusterIP
plane the Cluster DNS story (T6.2a) has nothing stable to resolve to.
定稿即交付 — LOCKED 与落地代码同 sprint (2026-11-22), 无 draft-only
窗口残留。

**Options.** (a) **bind 取舍** — `IP_FREEBIND` socket option vs 本机回环
别名 (`ip addr add`, the klipper/k3s precedent). The key difference is
route correctness for LOCALLY-ORIGINATED connections: FREEBIND only
solves bind (an agent connecting to its own ClusterIP still needs a
route), while a loopback alias solves bind AND the local route at once.
**定稿选回环别名** — not IP_FREEBIND (FREEBIND solves bind only, not
the local route; the alias solves both). (b) **分配面** — a standalone
allocator state machine vs 复用 apiserver CAS 自愈分配模式 (NodePort
现有分配模式同型). **定稿选后者** — NOT a standalone state machine.

**Decision (final, as-built S51).** **分配面归 apiserver**: `--service-cidr`
从 noop 转 wired, **server only** (agent 不持有), 经 S50 `wired_for_server`
先例接线 (与 `--tls-san` 同机制); 默认 `10.43.0.0/16`; 畸形 CIDR = server
启动 **fatal fail-closed**。分配 = in-CIDR 顺序分配 + 既有 Services 去重;
显式 `spec.clusterIP` 尊重 (in-CIDR 校验, out-of-CIDR → 422 拒绝); 冲突 →
CAS 自愈 (乐观重读 + 409 重试); headless (`clusterIP: None`) 保持 None
(为 T6.2a DNS 消费保留)。落点 `crates/apiserver/src/service_ip.rs` (290L)
+ `service_ip_tests.rs` (248L), 经 `collection.rs` create 路径接入
(`prepare_clusterip` + `heal_clusterip_collision`); flag 链 `ServerCmd`/
`ServerBind`/`lib.rs` → `ServeOptions.service_cidr` → `AppState.service_cidr`。
**承载面归 Router**: `crates/router/src/clusterip.rs` (302L, 仿 `nodeport.rs`
建模) + `clusterip_tests.rs` (117L) — 每 ClusterIP Service port 一个
openrusty reverse-proxy listener (**TCP only**; UDP 显式 out-of-subset),
ready-Endpoints 上游, reconcile 循环按 Q57 TrafficSplit per-endpoint 折叠
权重 (与 NodePort 面同一折叠算法; split 对象名 == 入口 Service 名, SMI
惯例), plane enable 旗标门控; headless/ExternalName Service 不分 VIP 不
监听。**bind = 回环别名**: `crates/router/src/vip.rs` (132L) — `ip addr add
VIP/32 dev lo` (klipper/k3s 先例), 幂等 (add-if-absent, 容忍 EEXIST),
drain 全量清理。接线 `crates/cli/src/runtime_server.rs` (plane spawn +
3-tuple drain: listeners + aliases) 与 `runtime.rs` (drain 顺序: nodeport
面先于 clusterip)。**single-node VIP scope**: 回环别名使 VIP 从节点自身及
任何能路由到它的客户端可达; 跨节点 ClusterIP 可达性 **不主张** (无
per-node ARP/routing magic) — 记录为 single-node scope (klipper parity:
k3s servicelb 无 L2 announcer 时同性质)。另记: 草案的「承载面仿
`router/servicelb.rs`」指针**作废** — servicelb.rs 被发现为死文件 (从未在
`router/src/lib.rs` 声明, 从未编译入 plane, 其测试从未运行), S51 删除;
实际模型是 `nodeport.rs`, Q41 的 L4 现实即 NodePort 面 (更正 Sprint 35
CHANGELOG 的文件级归属; announce 控制器 + nodeport LoadBalancer admission
为真, Q41 本身仍有效)。

**Consequences.** (+) ClusterIP 可达 → T6.2a DNS (S53) 有稳定数据源;
Q28 NodePort 面与 ClusterIP 面互补共存 (Router 内双平面, 无第二套数据面)。
(−) out-of-subset 逐字保留: sessionAffinity/UDP/externalTrafficPolicy
(Q28 原文其余部分继续有效); 回环别名触碰节点网络状态 (幂等 + drain 清理
纪律已由 ST9 验证); 跨节点 VIP 可达性 out (single-node scope)。

**S53 P1-④ 补记（2026-11-25, 主体批 commit 5cade90）.** 评审 P1-④:
`spec.ports[].nodePort` 整数字面 **0 = unspecified**（k8s 对齐）——
`explicit_node_port` 于 u16 转换前将 0 判为缺省返回 None: create 走
最低空闲自动分配（原为 422 out-of-range）, update ≡ 省略
（immutable-or-retain 语义一致, S52-D0 `validate_on_write` 复用同一
判定自动继承）, 0 不再计为已占用/冲突端口。非整数垃圾（字符串/浮点/
负数）仍报 integer 422。wire 无变化; golden fixture 无 nodePort:0 钉死。

## Q45 — T6.2a 内嵌 Cluster DNS: 宿主形态 (T6.2a / S43) (DRAFT 2026-11-15, S43 开工前定稿)

**Context.** T6.2a (S43) embeds a Cluster DNS server (纯 Rust):
`service.namespace.svc.cluster.local` A records from a Service/Endpoints
watch, headless answered with the endpoint IP set, everything else
forwarded to the host resolv.conf. Where the server LIVES is undecided.
This is a DRAFT — 定稿前不动代码.

**Options.** (a) 独立宿主进程 (a supervised peer binary). (b) kubelet
内嵌模块 — the Q34 precedent: flannel-rust runs INSIDE the agent process
via `crates/cli/src/flannel.rs`, no extra supervision surface. 草案倾向
kubelet 内嵌.

**Decision (草案).** kubelet 内嵌模块 (仿 Q34 flannel 内嵌先例)，pod 侧
经 CRI `PodSandboxConfig.dns_config` 下发 resolv.conf。必须在 S43
开工前定稿 (含 wire 格式/压缩细节与 D1–D5 验收面)。Q45 是新决策
(无被修订对象).

**Consequences.** (+) 单二进制教义 (Q1) 延续，无新增进程监督面。
(−) out: SRV/PTR/pod DNS (顺延)；DNS 服务端与 kubelet 生命周期耦合
(drain 顺序定稿时核对)。

## Q46 — T6.2b local-path 供给: 自写 provisioner+binder (T6.2b / S44) (DRAFT 2026-11-15, S44 开工前定稿)

**Context.** Q22 locked StatefulSet PVC semantics as "lazy binding until
T6.2" — in practice PVCs sat `Pending` forever because no storage plane
existed. The S44 slice (T6.2b) closes that hole. This is a DRAFT —
定稿前不动代码.

**Options.** (a) 真 upstream local-path-provisioner 移植 (Go 上游
behavior-faithful port). (b) 自写极简 provisioner+binder 控制器 — PVC
Pending → 在 data-dir 下造 hostPath PV → Bound；默认 StorageClass 注解；
schema 一行式注册 PersistentVolume/StorageClass (k8s_openapi 原生类型,
注册即得全套 generic CRUD/watch/SSA). 草案选自写.

**Decision (草案).** 自写极简 provisioner+binder 控制器。必须在 S44
开工前定稿；定稿时 Q22 原文加 `Superseded (Sprint N)` 注记 (Q46 修订
Q22: 原 "PVC Pending forever" 语义废弃，Bound 生命周期落地)。

**Consequences.** (+) STS 有状态负载获得真实可写存储 (T4.10 卷消费的
前置)。(−) out: 多节点亲和调度、VolumeBindingMode 延迟绑定 (upstream
parity 顺延)；自写面需 V1–V3 e2e 钉住生命周期语义。

## Q47 — T1.3a TLS serving + 静态 token 认证 (T1.3a / S50) (LOCKED 2026-11-21, S49 定稿, 代码 S50 开工)

**Context.** Q11 locked the API server as h2c/cleartext-only for v1
(intranet posture, Q38). The T1.3a slice adds opt-in TLS serving + static
bearer-token authn, revising that posture. 草案形成于 S46 开工前；S48 审计
把客户端选型列为待定项，S49 (2026-11-21) 定稿。T1.3a 代码按 S49 风险裁决
顺延 S50 开工（phase-4-closure §5 风险 1）。

**Options.** (a) TLS 客户端: rustls 直接依赖 vs 复用 openrusty TLS 面
(其 pin 前置了 forward adaptation 点 `gateway_config.rs`, Q38 P2-1)。
(b) serving 侧: axum-rustls vs openrusty listener 复用。

**Decision (LOCKED S49).** server 侧自签 CA + serving cert (`--tls-san`) +
axum authn 中间件 (Bearer 静态 token, `--token-file` 转 wired)。客户端 =
**rustls 直接依赖**：控制面 (apiserver + kubectl) 单一 rustls TLS 栈，与
openrusty 解耦——Q38 P2-1 的 `gateway_config.rs` 前置适配点留给 Router
数据面自身，不承担控制面 TLS（S55 再议）。Q11 原文修订注记随本定稿落库
(Q47 修订 Q11: h2c 明文仅为默认，TLS opt-in；golden h2c 基线不动)。

**Consequences.** (+) 控制面单 rustls 栈，避免双 TLS 实现的维护分叉；
TLS 面 + token 认证独立闭环规划为 S50 验收门 (`scripts/auth-e2e.sh`
A1–A6)，为 T1.3b RBAC (S50 认证链 I) 提供 authn 地基。 (−) golden 保持
h2c 明文基线 (TLS opt-in 旗标, 避免全量重基线)；kubeconfig 读取仅
server/CA/token 三字段起步，**完整 parity 显式弃置**入 phase-4-closure
§6 顺延台账（需求驱动再启，不设自动到期）；定稿时无已落地代码，S50
开工即受本条约束。

**S50 落地注记 (2026-11-21, T1.3 → done)。** 代码与证据门全量落地:
- **P1 TLS 栈**: `rcgen 0.13.2` 直依赖 (features crypto/pem/ring) 生成
  自签 CA + serving cert (SAN: localhost/127.0.0.1 + `--tls-san` 追加);
  serving 侧 `tokio-rustls` acceptor + axum `TlsListener` 适配; 客户端
  (kubectl) 同一 rustls 栈 (ring provider, TLS 1.2/1.3), **仅信任
  kubeconfig `certificate-authority`** (无 CA 的 https 是硬错误, 不落
  系统 CA); 单请求一握手 = 明文传输同型 profile。CA 落盘
  `<data-dir>/server/tls/server-ca.crt`。
- **P2 token 文件**: kubeadm CSV 格式 `token,user,uid,"groups"` (引号内
  逗号分隔组; 空组 = 空引号串)。`--token-file`/`INIT_PRO_TOKEN_FILE`
  wired (fail-closed: 文件不可读/畸形 → server 拒绝起播, 绝不退化为无
  认证服务)。
- **P3 节点侧透传**: kubelet/Agent 循环/嵌入 flanneld/flannel keeper 全部
  经 agent `--token`/`INIT_PRO_TOKEN` 携带 Bearer——kubelet `HttpJson`
  单一 chokepoint 注入 `Authorization`; flanneld 不读 env token, 走生成
  kubeconfig (`user.token`, `<data-dir>/agent/flannel/kubeconfig.yaml`)
  注入; server 端 bootstrap `cluster-admin`@`system:masters` +
  `system:node`(wildcard)@`system:nodes` 使 kubelet token 开箱可用。
- **G02/G03 字节 pin 保活**: RBAC 组与 ServiceAccount **仅在
  `--token-file` armed 时**注册进 discovery (`served_schema(auth_armed)`);
  默认 (未 arm) 姿态字节不变——golden G107-G113 交叉断言。
- 证据门: `scripts/auth-e2e.sh` A1-A7 (TLS/token/RBAC/discovery/fail-
  closed 真机) + golden auth.sh G107-G113 + parity t78-auth; Q59 子集
  边界 (webhook authn/authz、bootstrap token 轮转、x509 客户端证书、
  `kubectl auth can-i`) 显式留在子集外, 见 plan/01-api.md T1.3 卡。

## Q48 — 内置容器引擎: kiri 内核移植 + 单线程外部引擎进程 (T4.11–T4.13, 2026-11-16, LOCKED)

**Context.** T4.1 捆绑 containerd/runc 让 M1 按时闭环，但违反 Q1 的单二进制
教义：init-pro 仍要监督一棵 Go 运行时进程树，CRI 之外再套一层 shim。
P0/P1 spike 验证 Linux 原语已够用 —— `clone3` + 六个 namespace +
`CLONE_INTO_CGROUP` + `mount_setattr(2)` 在本机 (Linux 6.8, cgroup v2,
overlayfs, 真 root) 全绿，且 kiri 的 `kiri-init` 已用同一模型跑通。
本决策锁定自研引擎的形态，替代 "containerd 唯一后端" 的隐含前提。

**Options.** (a) 保持 containerd 唯一后端：零风险、但单二进制教义永远
打折。(b) 内嵌 runc：仍是 Go/CGO 进程模型，cgroup/namespace 语义没有
变简单。(c) 自研 kernel 层 + 自有 daemon/protocol，内核部分移植 kiri
`crates/kiri-init/src/linux/` (Apache-2.0)。选 (c)。

**Decision (LOCKED).**
- **进程形态.** 引擎是 `init-pro engine serve --socket PATH` 的**单线程**
  外部进程：无 tokio、无 `std::thread`、无 per-container 线程，因此
  `clone3(child_stack = 0)` 的 fork-like 语义安全。daemon 侧 (kubelet)
  用 tokio 客户端 (`crates/engine-client`) 走 Unix socket。
- **安全边界.** 全部 `unsafe` 只允许出现在 `crates/nsrun`；其余 crate 保持
  `#![forbid(unsafe_code)]`，nsrun 内每个 unsafe block 带 `SAFETY:` 注记，
  `#![deny(unsafe_op_in_unsafe_fn)]` + `#![deny(clippy::undocumented_unsafe_blocks)]`。
  `scripts/unsafe-audit.sh` 是门。
- **内核模型.** 照 kiri 的顺序：`clone3` 进
  `CLONE_NEWNS|NEWPID|NEWUTS|NEWIPC|NEWCGROUP|NEWNET` (+
  `CLONE_INTO_CGROUP`)；child 侧 `MS_PRIVATE` → bind rootfs → enter →
  `/proc` `/sys` `/dev` → masked/ro `/proc/sys` → `pivot_root` +
  `umount2(MNT_DETACH)` → hostname → cap-bounding drop → no_new_privs →
  setgid/setuid → execve；parent 用 start gate 放行 + `waitpid`，
  退出码 `128+WTERMSIG`，exec 失败 `126/127/255`。`clone3` 不可用或内核
  不接受 cgroup 时走已落地的回退阶梯：`clone3` + `CLONE_INTO_CGROUP` →
  无 cgroup 的 `clone3` → legacy `clone`（legacy 路径下 child 由
  `cgroup.procs` 迁入，且迁移仍在 start gate 放行之前完成）。
- **控制协议.** 内部帧 `u32 LE len | u8 kind | payload`（kind 1 JSON,
  2/3/4 raw stdout/stderr/stdin），`MAX_FRAME` 16 MiB。这是 daemon↔engine
  字节通道，不是 apiserver wire format；Q10 (JSON-only) 只约束 API 面。
- **默认与灰度.** 默认后端仍是 `containerd`；`--container-engine=builtin`
  在 P1 只做接受 + 告警，真实 kubelet 接线是 T4.12，镜像存储 + CNI 是
  T4.13。preflight 失败 (非 root / 无 `mount_setattr` / 无 cgroup v2)
  在 P1 是**硬错误**：`engine serve` 直接 exit 1，不做静默降级；
  "preflight 失败退回 containerd" 是 T4.12 选择路径 (kubelet 侧看到
  引擎起不来再选 containerd) 的责任，不是引擎进程自身的行为。
- **来源与许可.** kiri @ `7846fe0`，Apache-2.0；只移植 kiri-init 用户态
  Linux child 代码，`guest/kernel` (GPL-2.0-only) 永不触碰。归因见
  `NOTICE`，全量许可证文本见 `LICENSES/Apache-2.0.txt`。

**Consequences.** (+) Q1 单二进制教义落地：无 runc/containerd 依赖也能
起容器；内核面可单测 + E/B 门禁；unsafe 边界可审计；引擎崩溃模型是
显式进程，而不是嵌在 async runtime 里的线程。 (−) 仅 Linux cgroup v2
(5.12+) 且必须真 root (不承诺 user namespace)；P2/P3 仍需镜像层解包、
CNI 沙箱、exec/attach、日志落盘等工程；containerd 与 builtin 双后端
并存会扩大测试矩阵。明确 out: Windows、CRI gRPC server、pod 级
user namespace。

## Q49 — T4.12 进程池模型: 每容器一个 `engine serve` (2026-11-17, LOCKED)

**Context.** P1 (Q48) 的 engine 一次只持有一个容器（`engine busy` 拒绝第二
客户端）。T4.12 要让 kubelet 把整个 Pod 生命周期跑在 builtin engine 上，
必须先定进程拓扑：单 engine 多容器（需要 stream 复用/多路分解）还是
每容器一个 engine 进程。

**Options.** (a) 单 engine 进程 + 容器 id 复用连接（协议要 stream_id，
事件泵要按容器分发）；(b) 每容器一个 `engine serve`，socket 在
`<data_dir>/run/engine/<id>.sock`；(c) daemon 进程内嵌（违反 Q48 的
显式进程崩溃模型）。

**Decision.** 选 (b)：每容器一个 `engine serve` 子进程，socket 固定在
`<data_dir>/run/engine/<id>.sock`。sandbox = **pause 模式 engine**：以
`--pause` 起一个过 start gate 后永久等待信号的 PID 1（持有 fresh netns，
永不 execve）；同 Pod 容器以 `--netns /proc/<sandbox_pid>/ns/net` 加入。
kubelet 侧维护注册表（id → socket/pid/规格），重启后扫
`<data_dir>/run/engine/` 恢复视图。exec 连**容器自己的 socket**，复用其
主连接（「连接=容器」，见 Q50）。规模上限：**>64 容器/节点则重审**
（届时切单进程多会话 + stream_id v2，开新卡）。

**Consequences.** (+) 协议无需复用/多路分解；单容器崩溃面 = 单 engine
进程；注册表是纯状态可单测。(−) 每容器一个进程 + 一个 socket 的 fd/进程
开销；**agent 重启 = 容器全灭**（连接断 → engine 收割容器；接受并记录，
不做热迁移）；并发受 Q48 单线程约束，由多进程横向解决。

## Q50 — T4.12 日志与 exec 流: O_APPEND CRI 日志 + 单连接合并 exec (2026-11-17, LOCKED)

**Context.** kubelet 的日志读取侧（T4.6/Q39）按
`<log_root>/<uid>/<YYYY-MM-DD>/<name>.<attempt>.log` + CRI 文本行 +
copytruncate 轮转工作；builtin engine 必须落出**同构**文件。exec 方面
P1 帧协议只有 run 流（kind 2/3 raw stdout/stderr）。

**Options.** 日志：(a) kubelet 事件泵收集后落盘（泵断 = 日志丢）；
(b) engine 直接 O_APPEND 写 CRI 文本。exec：(a) 第二连接 + kind 4
attach 扩展；(b) exec 骑容器主连接，输出与容器流共用 kind 2/3。

**Decision.** 日志选 (b)：engine 按 kubelet 既有路径以 **O_APPEND** 写
CRI 文本行（`<RFC3339Nano> <stdout|stderr> <P|F> <line>`；attempt 由
kubelet 侧注册表按 `max+1` 决定，与读取侧 newest-attempt 挑选兼容，
copytruncate 轮转语义不变）。exec 选 (b)：**单连接、单在途**；
`Request::Exec` 在容器主连接上发出，`ExecStarted`/`ExecExited` 事件
定界，stdout/stderr 以 kind 2/3 复用承载（无 exec id 标签 —— 归属由
「连接=容器 + 单在途」保证）。**kind 4（stdin）双侧维持拒绝**；TTY、
attach、port-forward 明确 out（记 v2）。

**Consequences.** (+) 日志与 containerd 路径同构，读取/轮转面零改动；
泵断不丢已落盘日志；exec 无新帧 kind。(−) exec 输出与容器输出在 wire
上不可区分（单在途约束 + ExecStarted/ExecExited 定界弥补）；无 stdin
意味着 `kubectl exec -i` 的输入面 v1 缺失（记录在案）。

## Q51 — T4.12 节点级 one-shot 回退: boot 探针失败退回 containerd (2026-11-17, LOCKED)

**Context.** Q48 规定 preflight 失败时 `engine serve` 是硬错误，节点级
选择（builtin vs containerd）需要一个不依赖 kubelet 的启动期判定点。

**Options.** (a) 直接信任 preflight 的静态检查（root/mount_setattr/proc）
——覆盖不了 socket 布局损坏、SELinux 等运行态失败；(b) boot 时起一个
one-shot 探针 engine，ping 预算内应答才算 builtin 可用。

**Decision.** 选 (b)：agent/server 启动且 `--container-engine=builtin`
时，在 `<data_dir>/run/engine/_probe.sock` 起一个 `engine serve` 探针，
**15s ping 预算**；失败（含探测到 live engine 占位）→ kill + wait +
unlink 陈旧 socket → **回退 containerd 路径**并打 Warning。默认值仍是
`containerd`；默认翻转是 T4.13 N4 的 go/no-go 决定，不在本卡。

**Consequences.** (+) 运行态失败也有确定回退，节点永不因 builtin 探针
失败而 boot 失败；探针 socket 与容器 socket 同目录，布局单点。
(−) builtin 启动路径多一次进程起落（~百 ms 级）；「probe 成功但后续
容器起失败」仍按普通容器错误面处理（不整节点回退）。

## Q52 — T4.13 网络面: builtin Pod 网络 = host netns 共享 (2026-11-18, LOCKED)

**Context.** T4.12 (Q49) 之后 builtin 引擎的 pause 沙箱默认开 fresh
netns，但没有 CNI——无 IP 分配、无跨 ns 连通，podIP 恒缺失。T4.13 原
计划把 flannel CNI (Q34) 接进 builtin 路径；但 overlay CNI 意味着完整
数据面 (veth/网桥/IPAM/netlink/subnet.env) 与 containerd 路径几乎
重复，而 builtin 的定位是零依赖的最小运行面。

**Options.** (a) 内嵌 flannel CNI 进 builtin 路径 (完整数据面)；
(b) host netns 共享 = hostNetwork 语义：pause 沙箱不 CLONE_NEWNET，
worker 照旧 setns pause netns (= 宿主)，podIP = hostIP = `--node-ip`；
(c) 极简独立 netns + NAT (自研 mini-CNI)。

**Decision.** 选 (b)：builtin 模式下所有 Pod 共享宿主 netns。零桥、零
veth、零 netlink、零 IPAM、零新依赖、零新用户 flag (engine serve 内
部 flag `--host-net`，仅 kubelet builtin 路径在 pause 时传入)；nsrun
零改动 (`new_net=false && netns=None` 路径本就存在)。`EngineBackend`
存 `node_ip`，`list_sandboxes` 对 SANDBOX_READY 视图置
`ip=node_ip`；`build_pod_status` 增 `host_ip` 参 (=`cfg.node_ip`)，
故 podIP == hostIP == node IP。agent 在 builtin 下跳过 flanneld；
containerd 路径 (Q34 flannel) 一切不变。验收面：
`scripts/builtin-runtime-e2e.sh` N1–N5。

**Consequences.** (+) 零配置零鉴权即得全连通 (pod↔pod / pod↔host /
pod↔外网)；无 CNI 依赖面与重复实现；hostNetwork 用户语义可直测。
(−) **无网络隔离**：builtin Pod 与宿主共享端口空间，冲突由用户自负
(hostNetwork 同语义)；明示不做 netpol/egress 限制；`--node-ip` 默认
127.0.0.1 仅单节点正确，多节点需显式传真实 IP；进程/PID/mount/UTS/IPC
隔离保留不变 (只放开 net ns)。T4.13 网络面就此关闭，overlayfs 镜像
存储仍 open。
## Q53 — T4.13 镜像存储: 层目录共享 + per-container overlayfs (兜底融合复制) (2026-11-18, LOCKED)

**Context.** T4.13 镜像存储需要把 CAS 层喂给 nsrun `RunSpec.rootfs`。层只解包一次是
镜像存储的核心收益（磁盘 + 启动时间）；每个容器必须有可写面（upperdir）。

**Options.** (a) 层目录共享 (`<data_dir>/engine/images/layers/<algo>-<hex>` 只解一次，
只读) + per-container overlayfs (lower=层目录按 manifest 逆序, upper/work per-container
在 `<data_dir>/engine/images/views/<id>/`, mount 失败兜底为融合复制整树)；
(b) 每容器融合复制整树（无 nsrun 改动, 磁盘/启动重）。

**Decision.** 选 (a)：nsrun `RunSpec` 增 `overlay { lower: Vec<PathBuf>, upper: PathBuf,
work: PathBuf }` 可选项（unsafe 仅 nsrun 合例, mount 时序在 pivot_root 前）；overlayfs
mount 失败（内核不支持/权限）时兜底为把 lower 链按序复制进 upper（whiteout 已应用），
语义与 containerd 的 snapshotter 降级路径一致。层合并顺序 = manifest layers 顺序
（layers[0] 最底）。

**Consequences.** (+) 层共享零复制、容器启动只建空 upper；兜底路径保证任何内核可跑。
(−) nsrun 增加 mount(2) 面（既有 unsafe 边界内）；兜底复制代价 O(镜像大小)；upper/work
目录在容器删除时整体清除。

## Q54 — T4.13 解包/拉取依赖: tar + flate2(rust_backend) + zstd, HTTP 复用 kubelet 客户端 (2026-11-18, LOCKED)

**Context.** 解包 OCI 层需要 tar + gzip（docker 层）解压；已有 zstd（vendor 面）；拉取
manifest/blob 需要一个 HTTP GET。builtin 引擎的定位是零外部命令依赖。

**Options.** (a) `tar` crate + `flate2`（miniz_oxide 纯 Rust backend, 零 unsafe, 不引
zlib-ng/C）+ 既有 `zstd`；拉取走 `kubelet/src/http.rs` 扩展 raw-bytes GET（不引 HTTP
新依赖）；(b) shell out 系统 tar/gzip（与 builtin 零依赖定位冲突）。

**Decision.** 选 (a)：workspace 增加 `tar = "0.4"` 与 `flate2 = { version = "1",
default-features = false, features = ["rust_backend"] }`（caret-only, 锁入 Cargo.lock）；
kubelet 依赖二者 + `sha2`（digest 校验）。断点续传 (Range) 明确 out。

**Consequences.** (+) 纯 Rust 解包链, 无 shell-out; digest 校验单点在 kubelet。 (−) tar
crate 需自行处理 whiteout/路径穿越（不可信任镜像内容, M3 落地）；flate2 rust_backend
比 C zlib 慢（测试镜像量级可忽略）。
（M3 补注: per-layer whiteout 落地引入 nix 0.30 纯 Rust 安全封装, 仅用 mknod/setxattr, 已在锁内 — 零 C 编译依赖不变。）

## Q55 — T4.14 内置引擎宿主根文件系统: host-rootfs 默认 + host-mounts 注解 (2026-11-19, LOCKED)

**Context.** T4.13 后 builtin 引擎仍以「拉镜像 + overlayfs」为唯一路径, 但 init-pro
的节点内真实工作负载 (agent/opencoder 子进程、workflow step) 天然要宿主工具链与
文件系统; 每个系统 Pod 为此造镜像是反生产力的。同时 nsrun 的命名空间集已证明
kernel 隔离边界成立, 缺的只是「根文件系统从哪来」的默认值。

**Options.** (a) host-rootfs 默认: `--builtin-rootfs host|image` (默认 host), 容器
以宿主 rootfs 为 `/` (挂载表 NEWNS 是唯一可见性边界), Q53 镜像链降级为注解
`init-pro.io/rootfs: image` opt-in (代码保留); (b) 保持 image 默认, host 模式做
旁路注解; (c) 移除镜像链回归纯 host。

**Decision.** 选 (a)。要点:
- **根解析 (R1)**: host 模式 = 绝对目录 → staged `<data_dir>/engine/images/<name>`
  → legacy `<data_dir>/images/<name>` 兜底 → 可操作错误 (列明补救); image 模式 =
  T4.13 链不变。CAS 控制名 ({blobs,refs,layers,views,merged,tmp}/dotfile/多段名)
  由 `staged_name_ok` 拒绝 (staged 名与 CAS 目录不冲突)。
- **host 模式 EnsureImage = presence-only, 永不拉**; 注解 `init-pro.io/rootfs:
  image` 在 host 引擎下触发 create_container 内一次性拉取 —— 注解是显式 opt-in,
  这是刻意行为 (非缺陷)。image_config_body 合并仅 image 模式; host 模式 pod spec
  即启动真源 (无 entrypoint/cmd 融合)。
- **host-mounts (R3)**: `init-pro.io/host-mounts: container-path=host-path[:ro|:rw]`
  (逗号分隔, Q43 anyhome-mounts 同构; 空项容忍; 容器路径重复拒绝; 与 spec
  volumeMounts 碰撞 = pod 级拒绝, 该周期所有 create 被阻断)。挂载源存在性在
  create 前预检 (缺失 → Pending + log + CreateContainerConfig Warning 事件)。
- **权限姿态 (R2)**: 可见性 = 挂载表 (host-mounts 决定), 可写性 = uid/gid + caps
  (root 下 DAC 被旁路, 故 E2E 用缺失源而非 chmod-000 造败); caps 收敛非目标 →
  T4.4。bind 本身即权限裁决, 预检只为可操作事件。
- **命名空间集不变**: NEWNS|NEWPID|NEWUTS|NEWIPC|NEWCGROUP 锁定 (nsrun 零代码
  改动; 新增的仅是 spec 校验单测)。PID ns 是最后的 kernel 硬边界 (信号作用域 +
  /proc 可见性 + ns 树回收), 与 cgroup 限额 (T4.9) 正交。网络面沿用 Q52 host
  netns 共享姿态。

**Consequences.** (+) 系统 Pod 零镜像成本; 镜像链保留且显式化 (注解 opt-in);
nsrun 无改动面。 (−) host 默认 = 共享内核攻击面内运行 (与 Q52 姿态一致, 显式
接受); `--builtin-rootfs image` 成为 I 系列 e2e 的必带旗标; 引擎默认翻转
(go/no-go) 仍按 Q51 Sprint 43 裁定不变 (默认 containerd, builtin 整体 opt-in)。
**已知限制**: `init-pro.io/rootfs: /` 字面量不支持 —— nsrun 切根前的全新 sysfs
挂载与被复制的树冲突 (`engine-run: nsrun: mount sys: Device or resource busy`,
干净失败非脏挂); 要只读暴露宿主树, 用 fabricated rootfs + host-mounts
`/host=/:ro` (apply_bind 目录绑定走 MS_BIND|MS_REC, 树整体只读带出)。

## Q56 — T8.3 ServicePipeline/Build: embedded CI/CD build plane (2026-11-19, LOCKED)

**Context.** Layer 8 的交付面要回答「代码怎么变成集群里跑的镜像」。集群内没有
外部 CI (GitHub Actions/Jenkins), 而 init-pro 已自带镜像面 (T6.3 内嵌 Registry
v2, Q40) 与 kubelet 拉取链路 (T4.1)。同时宿主机本来就要有容器运行时: 构建面
若自造一个进程内 image builder (buildkit 等价) 会把 T8 变成另一个 T4, 与 M2
的收敛目标冲突。因此需要先定「构建在哪个进程、用谁的 docker、推给谁」。

**Options.** (a) controller 驱动宿主 `docker` CLI (build + save), 产物经内嵌
Registry push client (Q40) 推入, 另留「已构建 `docker save` 归档」的无 docker
路径; (b) 进程内嵌完整 image builder (Dockerfile 解析 + 层构建); (c) 把 CI 留在
集群外, init-pro 只提供脚本/文档。

**Decision.** 选 (a)。要点:
- **对象模型.** 一个 service = 一个 `init-pro.io/v1` `ServicePipeline` (短名
  `sp`): image repository+tag、source (docker context 或预构建 `docker save`
  归档)、可选目标 Deployment/container、构建旋钮 (`timeoutSeconds` 等)。
  `spec.paused` 冻结 `observedTag`。
- **一次 tag 变迁 = 恰好一个不可变 `Build` 子对象** (`<pipeline>-<tag>-<rand>`,
  controller ownerReference; GC 随父回收)。`Build.spec` 不可变, 历史即审计
  记录。
- **相位机.** `Pending`/`Building`/`Publishing` → `Succeeded` | `Failed`;
  同一 tag 的 `Failed` 被**按住** (不重建, 无失败重试环) —— 重试 = 换新 tag。
- **source 两态.** context 模式 = 宿主 `docker build` + `docker save` (经
  `executor` 有界超时 + `kill_on_drop`, 超时按数据上报而非 `Err`); archive
  模式 = 直接吃 `docker save` tar (tar + flate2/zstd, Q54), 完全无 docker 依赖
  (CI/进程内 e2e 路径)。
- **推送.** 复用内嵌 Registry 的 V2 push client (Q40); `registry_and_repo`
  区分 host-full (`host:port/repo`) 与 host-less (`repo`) 仓库名, 后者拼上
  `--registry-addr`。
- **成功后.** 尽力 patch 目标 Deployment 的容器镜像 (目标/容器缺失 = 告警,
  **永不**让发布失败), 然后置 `Promoted` + `status.observedTag`。
- **status 写入.** 全对象 CAS (v1 无 `/status` 子资源, Q10), 冲突交给 worker
  重排; `logTail` 截断到 4096 字符 (不是日志流)。scratch 在 `<data-dir>/pipeline`。
- **默认姿态.** 平面默认开 (与 anyhome/Registry 同款); `--disable-pipeline`
  → `ManagerConfig::pipeline = None` = 不建 informer、不起 worker、不 enqueue,
  行为与 T8.3 之前逐字节一致。

**Consequences.** (+) `kubectl apply` 一个 ServicePipeline 即完成「构建→推送→
改 Deployment 镜像」; 构建是一等对象 (可查、可 GC、随父级联); 归档模式让 e2e
无需 docker daemon; 关闭面零行为差异。 (−) context 模式依赖宿主 docker daemon
(单节点姿态, 多节点构建分发明确 out); 日志只有有界尾巴, 无流式; v1 无多架构/
构建缓存旋钮; 失败 tag 被按住, 重试必须换 tag; `Build` 与 `ServicePipeline` 两个
新 Kind 进入 namespace DRAIN_KINDS 与 GC 依赖表 (面变宽)。

## Q57 — T8.2 TrafficSplit: weighted one-click traffic switching (2026-11-19, LOCKED)

**Context.** NodePort 面的金丝雀/灰度需要把流量按比例分给两个 backend Service
(stable / canary)。主流做法是引一个 service mesh (SMI `TrafficSplit` CRD +
sidecar/proxy 数据面), 这与 Q32「Router 是内建数据面, 不是 addon」的教条冲突。
Router 已经有 resolver (Endpoints/Service reflect) + openrusty `apply_config`
请求间热重载 (T5.4/Q32), 缺的只是一个「权重从哪来」的对象。

**Options.** (a) 一等公民 `init-pro.io/v1` `TrafficSplit`, 由 Router resolver 折叠、
折算成 per-endpoint 权重后热重载 openrusty upstream; (b) SMI `TrafficSplit` CRD +
外部 mesh 数据面; (c) Ingress annotation 写权重 (字符串语义, 无对象、无 status)。

**Decision.** 选 (a)。要点:
- **形态.** `TrafficSplit` (短名 `ts`): `spec.service` = NodePort 入口 Service
  (listener owner), `spec.backends[]` 按名字引用 backend Service + 相对权重。
- **折叠.** `crates/router/src/traffic_watch.rs` 以 LIST→WATCH→re-LIST 折叠每个
  split 进 resolver 的 `splits` 表 (`SplitView`); 折叠幂等, 重放无害。
- **权重折算 (纯函数).** 丢弃 `weight <= 0` 的 backend (openrusty 拒收零权重
  peer); 每个 backend 的权重按端点均分并**放大 100 倍**, 余数确定性地给前
  `basis % n` 个端点 (首个端点先得), 零基数兜底为 1 —— 于是 `90/10` 在「2 个
  stable pod + 1 个 canary pod」下仍是 90/10, 与副本数无关。
- **热重载.** 权重变化 = openrusty upstream 重规划 + `apply_config`; peer 列表
  排序去重, 完全相同的编辑不触发 reload。零权重 backend 被丢弃, 入口 listener
  只在**所有** backend 都无 peer 时才 503 (Q28 parity)。
- **status.** 报 backends/phase; 数据面仍无 header/cookie pinning (v1 明确 out)。
- **无 mesh、无外部 CRD.** 复用 Q32 内核, 不引第二套数据面。

**Consequences.** (+) 一次 apply 即切流, 比例与副本数解耦; 折算逻辑是纯函数
(不进 async 调用栈) 可单测; 与 canary 面 (Q58) 是同一个原语; 无需 mesh 依赖。 (−) 只有
权重粒度: 无 header/cookie 粘滞、无按请求灰度 (需要 openrusty header match,
未来项); L4 NodePort 面按连接分配, 权重 0 的 backend 是被丢弃而非优雅摘流;
权重必须显式写在 backend 上 (无默认值推导)。

## Q58 — T8.5 canary release: pipeline-owned canary + stepped TrafficSplit (LOCKED 2026-11-19)

**Context.** Q56 让 ServicePipeline 能「构建 + 推送 + 改 stable Deployment 镜像」,
但直接改 stable 是 all-or-nothing。Q57 已给出按权重切流的一等原语, 缺的是
「谁按步骤推动权重、何时 promote/abort」。若交给用户手写 TrafficSplit, 每个
service 都要手搓一套时序, 且 promote 需要同时改 Deployment 与权重 —— 两步之间
有中间态。

**Options.** (a) pipeline 拥有 canary 面: 由 `ServicePipeline.spec.release` 驱动
canary Deployment/Service + 分步权重; (b) 复用 Argo Rollouts 语义 (外部 CRD);
(c) 只给用户 TrafficSplit, 不做自动 promote。

**Decision.** 选 (a)。要点:
- **D1 命名/归属.** `spec.release.canaryDeployment` / `canaryService` 为空时由
  pipeline 拥有 `<stable-deployment>-canary` Deployment 与
  `<stable-service>-canary` Service; 显式名字照用。子对象带
  `metadata.ownerReferences` (`controller:true`, `blockOwnerDeletion:true` →
  `ServicePipeline`) 与标签 `init-pro.io/pipeline: <name>`。目标名字上已有对象但
  缺该 owner/label 即**不收养**: condition `Ready=False reason=CanaryConflict`,
  phase `Failed`。
- **D2 selector 不相交.** canary Deployment 的 `spec.selector.matchLabels` =
  `{init-pro.io/pipeline: <p>, init-pro.io/canary: "true"}` (跨 revision 稳定)。
  canary pod template 标签 = stable pod-template 标签**去掉**所有出现在 stable
  Service `spec.selector` 里的 key, 再加三个标签: `init-pro.io/pipeline`、
  `init-pro.io/canary: "true"`、`init-pro.io/revision: <slug(tag)>`。canary
  Service 的 `spec.selector` = 这三个标签 (带 revision, 故新 tag 永不路由到旧
  canary pod)。stable Service/Deployment 除最终 promote 的镜像 patch 外**永不**被
  改。理由: k8s Service selector 是等值匹配的子集判定, 减 key 是唯一能把 canary
  pod 挡在 stable Service 外的办法。
- **D3 pause 时钟.** 只用 wall clock (无 cron): `status.rollout.since` 是
  RFC3339, 经 `crate::time::now_rfc3339`/`parse_rfc3339`; 步进挂在既有的 2s
  `PIPELINE_RESYNC` 上。每一步的 `pauseSeconds` 都要 soak, **含最后一步**, 之后
  才 promote。
- **D4 abort / 自动 abort.** 新增字段 `spec.release.abort: bool` (wire
  `abort`)。abort 在 reconcile 顶部检查 (早于建 Build) 且幂等: 删 canary
  Deployment+Service (NotFound 视为良性), 权重写回 `stable 100 / canary 0`,
  phase `Aborted`, `status.rollout` 清空, `observedTag` 不动, stable 镜像不碰。
  M3 **无自动 abort** (尚无健康信号); 清掉 `abort` 即从 step 0 重跑。
- **D5 副本数.** canary Deployment 克隆 stable 的 `spec.replicas` (缺省 = 1)。
  M3 不加新旋钮/旗标。
- **D6 promote 策略.** promote = patch stable Deployment 镜像 (既有
  `patch_target_deployment`) + 权重写回 `stable 100 / canary 0`, phase
  `Promoted`, `observedTag` 前进, `status.rollout` 清空。canary 子对象**保留**在
  权重 0, 供下一个 tag 复用 (回收路径是 abort)。
- **D7 revision 跟踪.** 新增字段 `RolloutStatus.revision: String` = 正在金丝雀的
  镜像引用; 滚动中 tag/reference 变化 → 从 step 0 重启。
- **D8 TrafficSplit 契约.** 指定的 TrafficSplit 必须预先存在 (Q57, operator
  拥有); 缺失 → `Ready=False reason=TrafficSplitMissing`, phase `Failed`, 不建
  任何子对象。控制器**只**改写 `spec.backends` 为恰好
  `[{stable, 100-w}, {canary, w}]`; `spec.service` 永不触碰。零权重 backend 已被
  Router 丢弃 (Q57)。
- **D9 非法配置.** 缺 `spec.target.deployment`、`trafficSplit`/`stableService`
  缺失或为空、`steps[]` 为空、权重越界 0..=100、canary 名等于其 stable 名 →
  `Ready=False reason=ReleaseInvalid`, phase `Failed`, 无重建环。

**现状声明.** API 类型自 M1 起已存在 (`PipelineRelease`/`ReleaseStep`/
`RolloutStatus`, `crates/api/src/pipeline.rs`; Sprint 46 追加 `spec.release.abort`
与 `rollout.revision` 两个 additive 字段), 控制器在 Sprint 46 落地:
`crates/controllers/src/controllers/pipeline/canary/{mod,plan,apply}.rs` (`plan.rs`
纯决策、`apply.rs` merge/ensure 辅助、`mod.rs` 步进机), 钩子在
`pipeline/mod.rs` (本 tag 的 Build `Succeeded` 之后), 进程内 e2e 在
`crates/controllers/tests/canary.rs`。

**Consequences.** (+) 一个对象覆盖「构建→金丝雀→promote/abort」, 中间态对用户
可见; 权重时序建立在 Q57 的纯函数折算式上, 可单测; canary 与 stable 靠 selector
减 key 严格隔离, 无 mesh、无外部 CRD。 (−) GC 依赖面变宽: `DEPENDENT_KINDS`
增加 `("apps","deployments")` 与 `("","services")`, 2s 兜底 sweep 现在要全集群
列这两类; namespace draining 的次序使 pipeline reconciler 里的
`metadata.deletionTimestamp` 守卫成为必需 (顺带修掉既有的 Build 复活竞态);
无健康门控的自动 abort —— crash-loop 的 canary 照样 promote; canary 克隆共享
pod 卷, RWO PVC 会阻塞调度 (已记录的 caveat); 发布活跃期 TrafficSplit 的
backends 归 pipeline 所有。

## Q59 — Scoped Kubernetes compatibility: discovery + supported golden matrix (2026-11-19, LOCKED)

**Context.** Q2 made full k3s/k8s protocol conformance the forward target. That
target is broader than the host-native AI vertical slice and includes expensive
surfaces with no current target workload. We need an honest compatibility
boundary that keeps existing clients and supported resources wire-compatible
without claiming unimplemented Kubernetes surfaces work.

**Options.** (a) Keep full conformance as the immediate forward gate and block
host AI on it; (b) narrow the forward target to an explicitly supported profile
while preserving upstream semantics within that profile; (c) define a custom,
loosely compatible API and silently accept unsupported operations.

**Decision.** 选 (b)。Q2 的 full-conformance **forward target 由此 superseded /
narrowed**；这是一项前向范围决策，不改写已完成工作或历史证据。
- **支持画像 = discovery + golden matrix。** 一个 API 只有在 discovery 中明确
  发布且被版本化 golden matrix 覆盖时才属于支持集。优先验证真实 `kubectl`
  与 `kube-rs`，以及普通 `apps/v1` Deployment 和 `core/v1`
  Service/Pod/ConfigMap/Secret 工作负载。
- 支持资源保留上游 **GVK/GVR、JSON representation、CRUD、watch、SSA、status
  与 error semantics**。范围变窄不意味着允许私有字段名、伪 status、简化
  conflict/not-found 错误或假 watch。
- 不支持的 surface 必须在支持矩阵中 documented，并通过 discovery/显式
  Kubernetes-style error 干净失败；**never fake success**，不得返回无实际效果
  的 2xx。Q10 不变：v1 transport 仍是 JSON-only，protobuf 不因此获得支持。
- Dynamic CRD registration、admission、API aggregation、protobuf、full RBAC 及
  其他广泛控制面能力不是 near-term 目标，除非一个明确的 target ecosystem
  workload 提出需求并随之扩展 discovery + golden matrix。
- **不作 full Kubernetes conformance claim。** T0.6 保持 done，并继续作为
  supported subset 的 merge gate；其绿色结果只证明矩阵内的兼容性。
- **Phase 3 golden addendum。** 实现时追加版本化 `phase3-v1` 用例，不重编号或
  改写历史 IDs：host-direct 严格三重选择正例及缺项/错误/非字面量 `/` 负例；
  禁止 containerd fallback；omitted/default 与 Always/IfNotPresent/Never 均产生
  零 image actions，显式非法 policy 被 Kubernetes-style validation 拒绝；GPU
  兼容发布/记账/互斥/restart/release 正例及不兼容后端、资源不足、非法资源和
  capability/backend 变化后的 stale nonzero publication 负例；Recreate 的
  runtime/GPU completion ordering 与 stalled teardown/release 负例；非法
  Deployment strategy 拒绝；inference NodePort 可达及未就绪时 endpoints 为空。
  API/client 断言使用真实 kubectl/kube-rs，必要时采集 privileged/runtime traces。
  既有 G70/G71/ST1-ST6 仅作通用数据面证据；T7.6 仍须验收 inference Pod 的
  Service/Endpoints 生命周期，不得以重复 generic proxy tests 替代。

**Consequences.** (+) 可以把实现投入普通应用与 host AI，同时让 kubectl/kube-rs
的真实交互保持可预测；支持边界可由 discovery 和测试审计。 (−) 矩阵外工具或
控制器不能假定为受支持，尤其不能因 server 接受对象就推断 controller 已兑现
语义。Q2 保留为历史 ADR，但其「所有现有工具/全协议」前向承诺不再成立；Q10
的 JSON-only 决定完全保留。T0.6 的状态和历史不变，golden matrix 的扩展仍是
每个新增支持面的必要交付物。

**S49 注记 (metrics-server 显式弃置):** metrics-server (metrics.k8s.io/
v1beta1) 经 S48 审计 + S49 复核显式弃置于前向支持集之外——无当前目标
工作负载 (HPA / kubectl top) 依赖它；若未来出现目标需求，按本决策的
addition 路径重新纳入（discovery 发布 + 版本化 golden matrix 交付物），
不作默认承诺。

## Q60 — T4.15 builtin host-direct performance profile inside a tenant-dedicated cluster (2026-11-19, LOCKED)

**Context.** k3as 是多租户 Agent sandbox 平台；支持的部署是一租户一集群，
且集群内所有节点专属该租户，Kubernetes Namespace 不是租户安全边界。Q55 已让
builtin 默认使用宿主目录 rootfs 和 host-mounts，但明确不支持
`init-pro.io/rootfs: /` 字面量。Tenant-dedicated 集群内的普通 host AI 推理需要
直接运行宿主 pinned 工具、读取预置模型并访问 accelerator `/dev`，fabricated
rootfs + 大棵 bind mount 会带来复制、挂载与设备语义摩擦。T4.11–T4.14 均保持
done，本决策只增加下一张普通推理性能画像卡；Agent action containment 由 Q63
独立提供。

**Options.** (a) 在 nsrun/engine 中增加严格选择的 literal-host-root 条件路径；
(b) 继续要求 fabricated rootfs + `/host=/:ro`； (c) 放弃 builtin host-direct，
让 AI/GPU 走 containerd + device-plugin。

**Decision.** 选 (a)。T4.15 **extends, does not reopen** T4.11–T4.14。
- **部署与用途边界。** host-direct 仅是 tenant-dedicated 集群内普通推理的
  性能画像，不是 Agent action-containment profile，也不是跨租户安全边界。
  一租户一集群、节点不跨租户共享是 hostile multi-tenant support 的必要但非
  充分条件；还必须有 audited/tested 的 per-tenant network 与 ingress/egress、
  storage/NFS/registry/workspace、API/NodePort/Router/kubelet endpoints，以及
  provider/broker endpoints 和 credential handling 隔离。控制未齐备并验证前，
  hostile multi-tenant deployment 不受支持。Agent 工作负载必须使用 Q63 的
  私有非宿主隔离，不得选择此路径。
- **先于镜像规划的显式选择。** kubelet sync 在任何 image planning 之前完成
  严格选择；只有 `--container-engine=builtin` + `--builtin-rootfs=host` + Pod
  annotation `init-pro.io/rootfs: /` 三者同时成立才选择字面量宿主 `/`。此路径的
  `image` 仅保留为 declarative/wire metadata，literal `/` 而非 image 选择 rootfs。
  create/restart/reconcile 对 workload 和 pause/sandbox 均不得发出
  `Action::EnsureImage`，也不得 pull、presence check、ref/CAS/layer resolve 或
  materialize、merge image config、copy/merge layers，或对 `/` 执行 `pivot_root`。
  omitted/defaulted 及显式合法的 Always/IfNotPresent/Never 均接受但行为无关；
  显式非法 policy 必须经 Kubernetes-style validation 拒绝，不得静默默认化。
  Q55 普通 host-directory 的 presence-only 行为不变。全局 engine 默认仍为 Q51
  `containerd`；AI/GPU vertical slice 必须显式选择 builtin。
- **必要 namespace 保留。** clone3 使用 NEWNS/NEWPID/NEWUTS/NEWIPC/NEWCGROUP，
  不加 NEWNET，网络仍是 Q52 host netns。字面量 `/` 的 child setup 先隔离 mount
  propagation，再在私有 mount namespace 中正确挂载新 PID namespace 的 `/proc`，
  避免污染宿主。
- **直接设备与既有控制。** 不以裁剪 tmpfs 替换 `/dev`，保留宿主 `/dev` 供
  tenant-dedicated 节点的普通 accelerator 推理访问。Q55 host-mounts 的 ro/rw
  与 T4.9 CPU/memory cgroup limits 保留；image rootfs 与既有容器路径不因此改变。
- **无 containerd 回退。** Q51 的普通节点级 boot probe 回退仍存在，但 host-direct
  或 GPU 请求绝不能转交 containerd，无论是否静默或显式。若选定能力不可用，Pod
  必须显式失败或 Pending 并给出可操作原因。
- **Golden 正负证据。** Q59 `phase3-v1` 必须覆盖严格三重选择正例、缺项/错误
  组合及非字面量 `/` 负例、所有合法 pull policy 在 create/restart/reconcile 对
  workload/pause 均零 image actions、显式非法 policy 拒绝，以及强制失败后无
  containerd 执行；同时证明 Q55 普通 host-directory presence-only 不变。

**Consequences.** (+) 预装二进制、本地 GGUF 和 GPU 设备无需镜像封装/复制即可获得；
Q55 的字面量 `/` 已知限制仅在这条显式路径上被解除，Q52/Q55 其余语义不变。
(−) 宿主根、宿主设备、host netns 与共享内核不提供 Agent 动作隔离或跨租户
边界；cgroup 和 ro bind 也不是完整安全隔离。Agent 必须使用 Q63 而非复用此
性能画像；拓扑隔离本身不构成 hostile multi-tenant 支持，所需 per-tenant 控制
未验收前该部署不受支持。Q51 默认和通用 probe 行为不变，但特殊请求的回退被禁止，
以避免用户以为获得 host-direct/GPU 却实际运行在无相应能力的后端。

## Q61 — T7.2 direct host whole-GPU scheduling, builtin host-direct first (2026-11-19, LOCKED)

**Context.** 旧 T7.2 草案把 GPU、Agent extender、分数 GPU/time-slicing 与策略面
混在一起。Host AI 的首个可交付需求更小：普通 Pod 以标准扩展资源申请一张宿主
GPU，并在 retry/restart/teardown 中保持正确的设备所有权。当前通用调度器还需
修正扩展资源容量和 assigned-Pod 记账，才能避免超卖。

**Options.** (a) containerd + 外部 Kubernetes device-plugin；(b) Agent 专属
scheduler extender；(c) 标准 extended resources + 宿主 inventory + builtin
host-direct 整卡分配。

**Decision.** 选 (c)，重定 T7.2 的范围与依赖为 **T4.15, T3.2**。
- **标准资源与发布资格。** 使用 `nvidia.com/gpu` 和 `amd.com/gpu`。只有兼容的
  builtin host-direct 节点（builtin engine + `--builtin-rootfs=host` + host-direct
  与 device capability checks 均成功）可发布非零 GPU capacity/allocatable；containerd、image
  rootfs、probe 失败或 incapable 节点即使有物理设备也必须 omit 或同时报零。
  GPU Pod 仍须显式携带 literal `init-pro.io/rootfs: /`，节点资格不放松 Q60 三重
  选择。capacity/allocatable 表示当前 eligible 整卡总量，assigned-Pod 用量只由
  scheduler/allocation ledger 记账。
- **唯一 Node status 构造路径。** canonical Node status construction 始终接收当前
  eligible GPU inventory，并用于 registration、每次 heartbeat、agent restart 和每次
  full status rebuild；当前 heartbeat 会替换完整 status，不得走丢失 GPU 字段的
  另一条路径。inventory/capability/backend transitions 后不得遗留 stale nonzero
  allocatable。
- **设备分配。** 只支持 whole-GPU logical exclusive allocation，并分别通过
  `CUDA_VISIBLE_DEVICES`/`HIP_VISIBLE_DEVICES` 选择设备。
- **通用语义修正。** Node patch 的 JSON Pointer 正确把 `~`/`/` 转义为
  `~0`/`~1`；absent extended-resource capacity 等于零；快照计入全部仍占用资源
  的 assigned Pods；request/limit 必须是整数，标准 defaulting 后 request 与 limit
  相等。无容量或不支持的后端必须显式 Unschedulable/Failed，不能假装可运行。
- **稳定、不重复。** 节点持久化分配记录以 Pod UID + container name 为键，保存
  resource/device ID；retry 与 agent restart 恢复同一分配，原子预留避免 double
  booking，teardown 完成后才 release。恢复已有记录先于接纳新 Pod。
- **首片边界。** 仅在 Q60 的一租户一集群、tenant-dedicated nodes 部署中用于
  普通推理；节点不得跨租户共享，GPU 直通不构成 Agent 或跨租户隔离。不做外部
  device-plugin，不做 containerd GPU/device-plugin，不做
  fractional/MIG/time-slicing。GPU 请求不得回退 containerd（无论是否静默）；没有兼容的
  builtin host-direct 节点就明确保持 Pending 或失败。
- **验证分层与关闭门槛。** fake tests 只能关闭明确标注的 fake subscopes，覆盖
  registration/每次 heartbeat/restart/full rebuild 及 inventory/capability/backend
  transitions、记账、互斥和释放。真实硬件在普通 CI 可 opt-in，但 recorded
  real-hardware evidence 是整个 T7.2 done 的强制条件：每一个声称支持的 vendor
  都必须有 inventory、独占、restart、release 证据；若同时声称 NVIDIA+AMD 则两者
  都要，否则必须明确收窄支持范围及 Q59 matrix。全 fake green 而无这些证据时
  T7.2 保持未完成；hardware-required 模式缺硬件/驱动必须 FAIL。

**Consequences.** (+) 普通 Deployment/Pod 无需新 API 即可申请 GPU，Q60 的设备
直通有明确且不超卖的分配面；稳定 UID/container 键消除同名重建和重试歧义。
(−) 这是逻辑独占而非硬件安全分区，`*_VISIBLE_DEVICES` 不是对抗恶意进程的
隔离边界。Q35/T7.1 的 Agent 资源和执行历史保持 done 且不变；T7.2 不再依赖
T7.1，Agent 专属 GPU 接入、fractional GPU 与广义无策略/策略调度工作全部
deferred。T3.2 也保持 done，上述修复是 T7.2 的通用语义补齐，不重开历史卡。

## Q62 — T7.6 llama.cpp-first inference on ordinary Deployment + NodePort (2026-11-19, LOCKED)

**Context.** Tenant-dedicated 集群内的普通推理 host-direct 与整卡调度完成后，需要
一个可演示、可验收的推理工作负载。
首片应证明标准 Kubernetes 工作对象能驱动真实 llama.cpp，而不是先创造模型控制面。
模型和推理服务均可由 operator 预置，网络下载在隔离/离线验收中不可依赖。

**Options.** (a) 新 Model/Inference CRD + 自动部署 controller；(b) 复用 Q35 Agent；
(c) 普通 Deployment → Pod + NodePort Service，宿主运行 pinned llama.cpp。

**Decision.** 选 (c)。
- **普通工作负载。** 用户提交 `apps/v1` Deployment 与 `core/v1` NodePort Service；
  Pod 通过 Q60 host-direct 和 Q61 标准 GPU resources 运行宿主预装、版本钉定的
  `llama-server`。不创建 Agent，不新增 CRD，不做 model-aware Router。
- **预置且不可变。** operator 预先将 GGUF 放在宿主文件系统并经只读
  host-mounts 暴露；binary/model 缺失或校验失败即明确失败。没有隐式模型、
  工具或运行时下载。
- **API 与流量。** 验收 `GET /v1/models`、`POST /v1/chat/completions` 的
  non-streaming JSON 与 streaming SSE，以及 `/health` readiness。现有通用
  NodePort 承载流量，Q52 host-netns 语义不变，Router 不理解模型身份。验收必须
  覆盖 inference Pod 的 Service/Endpoints 生命周期：无 ready backend 时 endpoints
  为空，ready 后经 NodePort 可达；G70/G71/ST1-ST6 仅是通用数据面证据。
- **Recreate 的 runtime/GPU 顺序。** 当前 Recreate 语义不足以保证单 GPU 安全。
  前向要求先将 old RS scale-to-zero，再把 new RS 保持在 zero，直到所有旧
  containers/sandboxes 的 **实际 runtime teardown** 和 GPU allocation release 均被
  明确 acknowledge/complete。RS spec/status zero、deletionTimestamp、terminating
  state 或 API Pod absence 都不是完成证明。具体机制由实现决定，但必须表示
  node/runtime + allocation ledger completion，并与旧 Pods/attempts 关联，stale ack
  不得解锁。停滞时 replacement 保持 zero 并给出可操作 status，只有 completion
  后才继续；验收必须包含 stalled teardown/release 负例。非法 Deployment
  `strategy.type` 必须拒绝，不得当作 RollingUpdate。
- **测试分层与关闭门槛。** 默认 CI 使用 fake OpenAI server + fake inventory；
  真实 pinned llama-server、本地只读 GGUF 和真实 GPU e2e 可在普通 CI opt-in、无网络
  下载，缺前置条件时清楚 SKIP，hardware-required 模式禁止 SKIP。fake green 只能
  关闭明确标注的 fake subscopes；T7.6 整体 done 必须有 recorded real run，验证
  health/models/chat/streaming 并证明 actual accelerator execution，否则保持未完成。
  当前没有此类真机证据。

**Consequences.** (+) 首片只组合已支持的标准对象和 host-native 能力，能分别用
CPU/fake 与真实 GPU 审计；Q59 的 supported profile 直接覆盖客户端交互，Q35 的
Agent 不被借用或重开。 (−) operator 负责模型和二进制预置、版本兼容与
容量；NodePort 是通用 L4 暴露而非模型网关。模型分发/GC、多模型路由、自动部署
controller、多副本跨卡编排均 deferred，不因本决策暗含实现。

## Q63 — T7.7 Agent action interception + Linux sandbox (2026-11-19, LOCKED)

**Context.** k3as 是多租户 Agent sandbox 平台；一租户一集群且节点专属该租户是
必要但非充分的部署前提，Kubernetes Namespace 不是租户安全边界。这个拓扑不能替代
Agent 动作边界：Agent、其工具与 subagent 仍可能执行 operator 明确禁止的动作。
T7.1/Q35/Q37 已完成 Agent 资源、调度与节点执行，其中 child-env credential
transport 是必须保留的历史事实；本决策扩展而不重开 T7.1。Q60 则只向
tenant-dedicated 集群中的普通推理开放宿主 `/`、host netns 与 `/dev`，不能用作
Agent action containment。T4.14 已有 nsrun/clone3/mount/cgroup 基础，应优先复用
Linux 能力，而不是创建平行 syscall/container stack。

**Options.** (a) 继续直接运行 opencoder，把凭证放入 child env，并依赖各工具自觉
遵守提示词；(b) 新建独立 syscall 容器栈和可任意出网的 generic provider proxy；
(c) 在 done T7.1 上增加中央 default-deny action gate，复用 done T4.14 的 Linux
隔离，并以 node-side、per-attempt、operation-allowlisted broker 访问 provider。

**Decision.** 选 (c)。T7.7 依赖 **T7.1, T4.14**，独立于 T4.15/T7.2/T7.6。
- **分阶段但不得伪关闭。** mediation-only central gate/provider broker 只是开发
  milestone，不是 Q63 containment 或受支持的 hardened mode，不能关闭 T7.7。
  containment 不可用时 hardened request 必须 Pending/Failed，不得以 mediation-only
  或任何 fallback 伪装成功。hardened completion 要求 Agent、opencoder、所有
  ordinary/custom/MCP/ssh tools、所有 subagents 及 descendants 全部处于同一 action
  gate 和同一 attempt Linux identity/namespaces/cgroups/cleanup 内，无一逃逸。
- **复用既有运行时。** 按适用边界复用 nsrun/clone3/cgroups/engine lifecycle；
  不新增第二套 runtime、daemon、syscall 或 container stack。
- **租户与 Agent 双层边界。** 一租户一集群及 tenant-dedicated nodes 是 hostile
  multi-tenant support 的必要但非充分条件，不能单独视为完整租户边界。支持还以
  audited/tested 的 per-tenant network 与 ingress/egress、storage/NFS/registry/
  workspace、API/NodePort/Router/kubelet endpoints、provider/broker endpoints 及
  credential handling 隔离为前提；这些控制未齐备并验证前 hostile multi-tenant
  deployment 不受支持。本决策只负责 Agent 动作边界，Q60 仍只是普通推理性能画像。
- **operator 所有的 default-deny。** baseline policy 仅由 node/operator 管理。
  AgentSpec、workspace config、resumed sessions、model/agent changes 与 subagents
  只能 narrow，不能 broaden、替换或禁用 baseline；每次恢复或变更都重新计算有效
  策略交集。任何扩权尝试均产生确定性 denial 与 audit event。
- **唯一中央分拨门。** ordinary tools、最早的 task/subagent dispatch、MCP、ssh
  与 custom tools 全部经过同一 tool-dispatch gate，early dispatch 不得成为旁路。
  subagent 继承与父 Agent 完全相同的有效策略，所有后续动作继续过门；拒绝结果
  必须确定、可测试，并记录不泄露 secret 的 audit event。
- **Linux 强制隔离。** 每个 Agent attempt 复用 nsrun/clone3，使用私有非宿主
  rootfs + mount namespace 和 fresh network namespace；以非 root uid/gid 运行，
  强制 `no_new_privs` 与空 capability sets。仅允许 operator-allowlisted selective
  host binds；runtime/tool binds 只读，只有指定 workspace/scratch 可写，Agent config
  不得扩大这些权限。`/dev` 必须是 fresh minimal safe nodes，绝不暴露宿主 `/dev`。
  sandbox 不得看见 host container/engine/Docker/CRI/其他 runtime-control sockets；
  per-attempt broker Unix socket 是唯一有意保留的 host IPC exception。禁止 literal
  host `/`、宿主 network namespace 及 Q60 host-direct，不靠提示词代替内核边界。
- **强制资源与后代回收。** cgroup v2 CPU、memory、PIDs limits 均为强制有界值，
  不允许省略为 unlimited。Agent/opencoder/tool/subagent 共享受控 attempt 生命周期；
  成功、失败、超时或取消都要可靠清理全部 descendants，再回收 cgroup、socket
  和挂载。
- **broker 而非通用代理。** provider 访问仅可经 node-side operation-allowlisted
  broker，每次 attempt 分配独立 Unix socket 并在结束时撤销。broker 只执行配置的
  provider operations，在上游连接侧注入凭证，不提供任意 egress，也不是 generic
  proxy。Agent、opencoder、ordinary/custom/MCP/ssh tools 与 subagents 的环境都
  没有 provider credentials。
- **有限的 supersede。** Q63 仅对 sandboxed Agent path supersede Q35/Q37 的
  child-env credential transport；Q35/Q37 的完成状态、历史实现与证据原样保留。
  sandbox 必需能力不可用时，Agent 保持 Pending/Failed 并给出可操作 reason；
  禁止 direct-process、host-root、host-network 或 containerd fallback。
- **可分层验收，不可缺项。** unit/fake tests 必须覆盖 policy inheritance、只能
  narrow、config/resume/model/agent/subagent override resistance、所有 tool/dispatch
  入口、确定 denial 与 audit。特权 Linux integration 必须覆盖 filesystem/network/
  capability/cgroup/descendant containment，逐一验证 allowed/denied binds、
  workspace/scratch 外写入拒绝、fresh minimal `/dev`、host/runtime sockets 不可见，
  以及所有 descendants 与 Agent/opencoder/tools/subagents 同 gate/identity/namespaces/
  cgroups/cleanup。provider 测试只使用 fake local provider 与 Unix-socket broker；
  secret canaries 不得进入 env、argv、logs/audit；凭证只在 broker 上游侧注入。
  containment 或 credential confinement 必须 fail closed，现有 gates 保持绿色，
  且任何验收都不得依赖外部 service。

**Consequences.** (+) Agent 动作在工具与 subagent 边界统一可审计，配置与会话无法
自行扩权；复用已有 Linux/nsrun/cgroup 工作减少第二套运行时，凭证不进入可被
Agent 读取的环境，fake 与特权测试分别提供可重复证据。 (−) 节点必须提供私有
rootfs、fresh netns、非 root 身份、强制 cgroup v2 与 broker 等完整能力，否则只能
明确 Pending/Failed，不能以 host-direct/containerd 伪成功。operator 负责 baseline
allowlist 与资源上限；Q63 不把 Namespace 变成租户边界，也不改变 Q60 普通推理
性能画像的部署约束。

**S49 落地注记 (D7 收口):** T7.7 D7 已落地——opencode 以进程内 tool-dispatch
seam 运行于独立 staged 二进制 `agent-runner`（`crates/agent` `[[bin]]`，绝不
链接进 init-pro 多调用二进制，保住 Q36/D1 libsql 不变式）：默认拒绝策略经
`runner_gate` 在 native 分发点包裹 tool registry（GatedTool 复核含 hallucinated
与 `mcp__` 名的每次分发，`Agent.tools` 广告过滤不再是唯一闸门），父会话
registry 原样传入子会话，subagent 继承同样受限；`task` spawn 绕过 registry
map 的路径在广告层拦截，被强制 spawn 的子会话整体处于已门控 cgroup 约束内。
节点级强制硬化开关 `--agent-require-sandbox` 到位：无沙箱承载能力的 Agent
被拒绝为 Pending + `SandboxRequired` 条件（fail closed），禁止 host-direct/
containerd 伪成功。遗留（可用性差距，非遏制差距）：provider broker 的
op-protocol ↔ OpenAI HTTP 真实桥接仍为 stub——fresh netns 下无 egress，
沙箱化 Agent 的真实上游调用待 provider 桥接兑现（phase-4-closure 台账）。

## Q64 — T4.4 NetworkPolicy supported-subset: 上游 1:1 API 面 + agent 侧 in-tree iptables 执行面 (T4.4 / S52) (LOCKED 2026-11-22, S52 定稿即交付)

**Context.** T4.4 是 Layer 4 网络面最后一个 TODO。Q59 已把前向交付收敛为
「有限支持子集 + discovery 发布 + golden 矩阵覆盖」；Q52 锁定 builtin
Pod 网络 = host netns 共享（builtin 节点无从 policing）；k3s 以
`--disable-network-policy` 让位上游 netpol 控制器，而 init-pro 无 addon
控制器可选，需要自带执行面。定稿即交付——LOCKED 与落地代码同 sprint
（2026-11-22），无 draft-only 窗口残留。

**Options.** (a) **数据面** — flannel-core trafficmngr（link_repos 钉定
树, masq 整形 API）vs in-tree iptables shell（`masq.rs` 先例）。
trafficmngr 评估后弃：其 API 为 flannel masq 整形而非任意 per-pod 策略
程序，且不触碰钉定树是 Q38 纪律。**定稿选 in-tree iptables。**
(b) **匹配策略** — ipset 风格集合 vs 逐 pod 枚举 allow-list。**定稿选
逐 pod 枚举链**（不引 ipset 依赖；规则量 O(被隔离 pod × peers) 为记录
在案的子集边界）。

**Decision (final, as-built S52).**
- **API 面上游 1:1**: 手写 `networking.k8s.io/v1` NetworkPolicy 类型
  （`api/src/networking/network_policy.rs` 196L +
  `network_policy_rules.rs` 365L）注册进 discovery——**golden G31 字节
  锁步变更**，fixture + suite 断言 + 注册**同 commit 原子落地**（T0.6
  合并门纪律：任何 API 增补都是字节级合并门事件）；LabelSelector 匹配
  委托 `api::label_selector_matches`（controllers 复用，零重复）。
- **子集口径（Q59）**: ingress+egress 隔离；peers = podSelector /
  namespaceSelector / ipBlock（cidr + except，CIDR 减法在
  `cli/src/netpol_cidr.rs`）；TCP/UDP 数字端口；policyTypes k8s 默认；
  空 rules = default-deny。**OUT**（admission 422）: SCTP、命名端口、
  非法 peer 混排（如 podSelector+ipBlock 同一 peer）。
- **执行面在 agent**: `netpol_keeper.rs` 5s 全量重生成 LIST 循环
  （pods/namespaces/networkpolicies）+ `netpol_compile.rs` 纯函数程序
  编译；数据面 in-tree iptables shell（`netpol_iptables.rs`）:
  `INIT-PRO-NPOL-FWD` 分发链自 FORWARD pos 1 跳入
  （bridge-nf-call-iptables 路径承接同节点桥接 pod 流量），per-pod
  `INIT-PRO-NPOL-{I,E}<12hex>` 链 = conntrack ACCEPT + allow 规则 +
  终端 REJECT（**deny = REJECT fail-fast，非 DROP**），每次重生成单条
  `iptables-restore --noflush` payload（幂等显式 `-F`；
  `iptables-save` 陈链剪枝），LIST 失败降级 WARN（执行停在最后好
  程序，agent 绝不下线），shutdown teardown 零残留。
- **排除（Q52）**: builtin-engine 节点与 hostNetwork Pod 永不被
  policing；无 CNI（flannel none）= 不执行。
- **Flag**: `--disable-network-policy` wired 在 `agent`（默认开），
  `server` 侧 accept-noop-warn（k3s 把它拼在 server；init-pro 的执行面
  是 agent 本地的）——parity t80。

**Consequences.** (+) Layer 4 网络面 TODO 随 T4.4 收口；上游 1:1 wire
类型 + k8s 默认语义使现有 manifest 直接可用；agent 本地执行面无 addon
控制器依赖；e2e 证据 `scripts/netpol-e2e.sh` NP0-NP4 **12/12**（基线 /
default-deny+子集准入 / 选中放行 / egress ipBlock 白名单 / 剪枝+零残留
teardown），append 进 all-gates.sh（gate-8），root+vendor+iptables 缺失
→ SKIP（Q25）。(−) 规则量 O(被隔离 pod × peers) 逐 pod 枚举链（无
ipset 依赖）——supported-subset 边界记录在案；链命名空间 host-global →
一 host 一 agent 姿态（同 host 第二个 agent 会覆写共享链——测试环境与
遗留验收部署的碰撞观察，非产品缺陷）；golden discovery 字节此后是任何
API 增补的合并门事件。

**S53 补记（评审修复批 P1-①/P1-⑤）.** (a) **写路径准入补全**:
`apiserver do_patch` 在 CAS 写前对 FINAL merged 对象补跑
`api::validate::admit`（与 create/replace 同一单点）——此前 PATCH /
apply-patch 可绕过子集准入走私（如 SCTP 端口 merge-patch 入库）；
集成测试 `patch_runs_subset_admission_netpol` + netpol-e2e NP1 patch
门（SCTP merge-patch → 422）防回归。(b) **编译器 fail-closed 收紧**:
`netpol_compile::port_entries` 未知 `protocol` 由静默按 TCP 编译改为
skip + WARN（杜绝事故性 tcp 放行；TCP/UDP/缺省之外的值整条跳过）。
(c) **稳定性**: `netpol_iptables.rs` 全部 exec 套 5s `EXEC_TIMEOUT`
（`kill_on_drop`，超时归一为 `TimedOut` 走既有 WARN 降级路径，不挂
resync/drain），keeper drain 对齐 `REGISTRY_DRAIN_TIMEOUT` 的 10s 有界
写法（超时 abort + WARN，不挂死 agent 退出）。

## Q65 — T4.5 代理隧道: agent 主动拨号反向中继（长轮询, 单请求/响应对齐 Q31） (T4.5 / S52) (LOCKED 2026-11-22, S52 定稿即交付)

**Context.** T4.5 余量 = 代理隧道（节点注册/心跳已在 T4.2 Scope A
落地）。跨节点 logs/exec 需要 apiserver→kubelet 可达性；Q31 已锁定单
请求/响应语义（kubectl 不变，无客户端流式）；k3s 式全多路复用
remotedialer 与 WebSocket/SPDY 显式 OUT（不重开 Q31）。两节点默认陷阱:
`--node-ip` 未设时 node2 广告 127.0.0.1。定稿即交付——LOCKED 与落地
代码同 sprint（2026-11-22）。

**Options.** (a) k3s 式 remotedialer 全多路复用反向隧道（OUT，重开
Q31）；(b) WebSocket/SPDY upgrade 通道（OUT，同上）；(c) **agent 主动
拨号的反向中继，plain HTTP + JSON 长轮询**。**定稿选 (c)。**

**Decision (final, as-built S52).**
- **协议**（JSON-only Q10；认证 = agent 的 Q47 bearer token，
  `Target::NonResource` → AUTHENTICATED）: `POST /api/v1/nodes/{name}/
  tunnel/register`（心跳锚）· `GET .../tunnel/next`（server park 25s；
  204 空 / 200 `{id,method,path,headers,bodyB64}`）· `POST .../tunnel/
  responses/{id}`（`{status,headers,bodyB64}`）；wire 类型
  `apiserver/src/tunnel_wire.rs`，b64 helper `common/src/b64.rs`。
- **Registry**（`apiserver/src/tunnel_registry.rs`）: park=活性
  （LongPollGuard drop 即 unpark），代际守卫防陈旧 unpark，死泊自愈，
  pending id→oneshot，`RELAY_TIMEOUT_SLA`=10s。
- **缝**: `kubelet_hop::call_kubelet_relayed`——**DIRECT 优先**；仅
  transport 错误回退中继（HTTP 错误状态不回退）；回退失败与直连失败同
  502 形状（Q31 客户端透明）。单节点默认姿态零变化（直连永远赢）。
- **Dialer**（`cli/src/tunnel_dialer.rs`）: agent run path 默认开，
  退避 1s→15s 指数帽，客户端长轮询帽 35s > server 25s（免竞态），
  重注册 SLA ≤30s（SIGSTOP/SIGCONT 实测 5-6s）；`--disable-proxy-tunnel`
  退出（parity t81）。
- **Node 地址修正**: `--node-ip` 未设 → `kubelet::detect_outbound_ip(
  server_url)`（bindless UDP connect 探 apiserver host，兜底
  127.0.0.1）——node2 不再广告 loopback。

**Consequences.** (+) 两节点默认姿态下跨节点 logs/exec 可用
（acceptance-2node **16/16 两轮**：T1 exec 穿 kubelet 直连端口 INPUT
DROP 经中继 / T2 隧道断开重连 SLA / T3 `--disable-proxy-tunnel` 回归
直连 exec + Ready=True）；隧道永不承重（处处直连优先）；node 地址修正
顺带消掉 loopback 广告陷阱。(−) exec 流式不穿隧道（单请求/响应 =
Q31 范围，standing）；中继跳加长轮询交接延迟；无 mTLS（Q21 明文前提）。

**S53 补记（评审修复批 P1-②）.** `submit` 的 pending 响应者条目加
RAII Drop-guard（`PendingGuard`，`submit` 改取 `&Arc<Self>` 以便
guard 持有注册表句柄）：submit future 在 SLA 前被取消（HTTP 调用方
断连、axum 丢弃 handler）时由 Drop 清理 pending 条目——原仅超时分支
显式清理，取消路径泄漏至「可能永不到达的 agent 响应」；单测
`cancelled_submit_cleans_pending` 钉住。隧道 park 身份绑定（评审
P1-③，register 发一次性 park 凭据 + next/responses 验证归属）留待
S53 主体批：涉及 wire 字段扩展，先补 Q65 决策修订再动协议。

**S53 主体批修订（评审 P1-③ park 身份绑定, 2026-11-25 定稿即交付）.**
威胁面（原状）: 隧道三路由的认证姿态是「任何合法 bearer token 即可通过」
（`tunnel_http` 头注记, `/version` 姿态）——持有合法 token 的任意身份可
抢占他节点的 park 座位（顶掉真 dialer 的长轮询）或向他节点 pending 注入
伪造响应（同 token 多节点/凭证共享面）。**修订**: `tunnel/register` ack
增发 park 凭据 `parkToken`（getrandom 128-bit hex, 每次 register 轮换,
单一活跃值——「一次性」= 每次 register 单次下发、旧值即刻失效）;
`tunnel/next` 与 `tunnel/responses/{id}` 以 query 参数携带凭据,
apiserver 在 park/complete 的**同一临界区内**验证归属（锁内原子
校验+落座, 无 TOCTOU）; 凭据缺失或不匹配一律 **401 fail-closed**
（剥离参数与凭据不符不可区分, 不做 absent 容忍）。wire 兼容:
`parkToken` 为 ack 新增字段 + 两路由新增 query 参数, 旧端忽略未知
字段; 但新 server 对缺失凭据 fail-closed——版本偏斜（旧 agent + 新
server）下隧道面 401（直连优先、隧道仅 transport 回退的 Q65 姿态
不变; 单一二进制整群升级模型下偏斜窗口 = 滚动升级瞬时, 记录在案）。
明文 h2c 下凭据可嗅探（Q21 无 mTLS 的 standing 限制不因本修订解除
——本修订把门槛从「任何合法身份」提高到「最新 register 的 dialer」,
非防嗅探）。Q31 单请求/响应语义不变。

（落地 2026-11-25, S53 主体批 commit f63be2e: `RegisterAck` wire 类型 +
`parkToken` query 参数 + `park_checked`/`complete_checked` 锁内原子验证,
`crates/apiserver/tests/tunnel_auth.rs` 9 项 401 劫持拒绝矩阵; 上述
fail-closed/单活跃轮换/版本偏斜姿态逐款兑现, Q31 单请求语义与 Q47
bearer 认证层未动。）

## Q66 — 工作区 NFS 平面去 anyhome 实体化: nfsserve 直连 + 内嵌 DirFs (T7.4 rebase / S54) (LOCKED 2026-11-25, S54 定稿即交付)

**Context.** 用户指令 (2026-11-25): 「k3as 内置 rdb 存储, 集群内部的存储
都可以直接使用它! 完全下线掉 anyhome, 使用 link_repos 下的 nfs 做内置
存储能力」。平台侧结构化存储 rdb-only 已由 Q33 完成 (`rdb://` opt-in
durable, 平台默认 embedded); 检视 anyhome 实体层的实际消费面后发现:
init-pro 消费的只有其 NFSv3 文件平面 (`/workflow` / `/agent` / `/pod`
子树 + 单口 NFS/MOUNT/portmap 复用), 而 any-home crate 的记忆/检索层
(MemoryDb/libsql、DummyEmbedder、`/search` RPC、`anyhome.db`) 在
init-pro 内**零消费者**——从未进入任何 TODO 或用户面契约。与此同时该
实体带来三条结构成本: libsql 成为第 7 个 mandatory pin (Q38 收口时
发现); Q36 D1 的双 libsql 符号冲突 workaround 以 vendor 副本为前提;
嵌套 pin / also_link / 词法路径恒等教义整条链只为 any-home 存在。
nfsserve 仓库的 `examples/mirrorfs.rs` (625 行, BSD-3-Clause, HuggingFace)
是完整的生产级目录 VFS (FSMap/FSEntry/RefreshResult: intaglio
SymbolTable 字符串驻留 + AtomicU64 fileid + tokio fs, 每次
lookup/readdir 双 stat 的 mtime 失效刷新, 全写路径 setattr/write/
create/remove/rename/mkdir/symlink), 正是「link_repos 下的 nfs 做内置
存储能力」所需的可移植件。

**Options.** (a) 保留 any-home crate、仅停用 MemoryDb —— 弃: 实体与两个
pin 留存, 结构成本一条不减。(b) opencode store 换 rdb —— **明确不在
范围**: opencoder 的 libsql 走 crates.io 0.9.30 (其 store 既有形态,
mysql 为默认关闭的可选 feature), 用户指令指向的是「集群内部存储」平台
面 (已 rdb), 不是 opencode 内部 store。(c) 移植 mirrorfs 为 cli 内嵌
DirFs, `NFSTcpListener` 直挂导出 `/`, anyhome 实体死亡 —— **选**。

**Decision (final).**
- **用户面契约 100% 保留 (「anyhome」降级为该平面的历史命名)**: flags
  `--anyhome-addr` (默认 `127.0.0.1:11111`, env `INIT_PRO_ANYHOME_ADDR`)
  / `--disable-anyhome` (env `INIT_PRO_DISABLE_ANYHOME`); annotations
  `init-pro.io/workspace` / `init-pro.io/anyhome` /
  `init-pro.io/anyhome-mounts`; volume 名 `anyhome` /
  `anyhome-<sanitized>`; 盘上布局 `<data-dir>/anyhome/fs` +
  `workflow/` `agent/` `pod/` 根; keeper 脚手架 (skills/tools/mem/
  prompt.md 喂 runner `--prompt-file`, Q37) 与
  `spawn_anyhome(data_dir, addr, store, shutdown)` 签名; Q43 注入链
  (create/apply/replace prepare) 与 kubelet 消费面 (父级 export 挂载 +
  `vers=3,tcp,nolock,noacl,port=P,mountport=P`) 全部零改动。命名不改
  是契约兼容的一部分——flags/annotations 是已发布的用户面。
- **新底座**: `crates/cli/src/anyhome_vfs.rs` (FSMap 内核移植) +
  `anyhome_dirfs.rs` (`DirFs` + `NFSFileSystem` trait impl 移植) +
  `anyhome.rs` 收敛为装配层 (`NFSTcpListener::bind(addr, fs)` +
  `with_export_name("/")` 单 TCP 口, serve 循环保留 `tokio::select!` +
  `Shutdown` drain 姿势——nfsserve 无 cancel seam 的 Q36 理由仍成立)。
  移植源在模块头 + NOTICE 归属 (mirrorfs/nfsserve, BSD-3-Clause, 仿
  kiri 条目格式, 全量许可证文本入 `LICENSES/BSD-3-Clause.txt`); `intaglio` 1.6 成为 cli 直接 crates.io 依赖 (在 nfsserve
  里它只是 optional demo feature)。
- **实体死亡**: any-home 与 libsql 两个 pin、root `Cargo.toml` 的
  `anyhome` path dep、`crates/cli` 的 anyhome dep、`MemoryDb`/
  `anyhome.db`/`EMBED_DIM`/WAL checkpoint/`anyhome::cli::port_of`、
  逐层镜像上游 main.rs 的装配。存量 `<data-dir>/anyhome/fs` 是普通
  目录树, DirFs 直接复用存量字节; 遗留 `anyhome.db` 成为无害死文件
  (不再读写)。AUTH_SYS identity patch 保留不动 (Q37 caller-identity,
  canonical copy 在本仓库 + marker `src/identity.rs`)。
- **T7.4/T7.5 状态不变**: 语义与用户面不变、底座更换, 两行保持 done
  (Q36 supersede-in-implementation + Q43 rebase note 记录承接); counts
  43/4/7 不变。Q36 D1 约束保留 (server 二进制不链 `agent`, 路径 SSOT
  在 `common`) 但 rationale 改写为 link-graph hygiene——vendor libsql
  副本消失后符号冲突前提不复存在。

**Consequences.** (+) pin 7→6, 嵌套 pin/also_link/词法恒等教义整体
退役, bootstrap 冷克隆面简化; server 二进制的 libsql 链接面归零
(opencoder 子进程侧不变, 其 store 仍走 crates.io libsql); 双 libsql
符号冲突类问题永久关闭。(+) 数据面无迁移成本 (目录直通)。(−) DirFs 是
in-tree 移植: nfsserve 上游 mirrorfs 演进不再自动跟进 (适配面记录在
案; 单口/vers=3 契约由 e2e A1/P2/K1/P3-P5 + apiserver/kubelet 钉死
测试零改动复跑保障)。(−) mirrorfs 继承 demo 出身的特性——VFS 语义
(每次 lookup 双 stat 刷新、READDIR 全量重列) 与 any-home RealFs 同为
目录直通, 无回归预期, 但性能特征未做专门基准。Gate: workspace tests
(基线 1509/0)、golden 75/0、parity 31/31、clippy 0、
`scripts/anyhome-e2e.sh` A1/P2/K1/P3-P5。
