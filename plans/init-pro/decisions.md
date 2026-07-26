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
`init-pro-cli`: resolution order env `INIT_PRO_CONFIG_FILE` →
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
