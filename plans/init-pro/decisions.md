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
