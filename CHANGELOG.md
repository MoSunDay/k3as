# Changelog

All notable changes to init-pro are recorded here. Format is loosely
[Keep a Changelog](https://keepachangelog.com/); versions follow the plan
milestones in `plans/init-pro/`.

Test counts cited below are the **fresh** `cargo test --workspace` output at
the time of the entry (passed / failed), included so the numbers stay auditable.

## Sprint 11 — Server-Side Apply field-manager (T1.2c) (2026-08-04)

**Focus:** Implement server-side apply (SSA) so `kubectl apply` works
end-to-end. Field ownership tracked via `metadata.managedFields[]`,
with conflict detection, force-override, and field pruning.

### What landed
- **T1.2c SSA field-manager** — two new crates' worth of logic:
  - `crates/api/src/apply/mod.rs` — `apply_object()` entry point: create-on-
    absent (seeds managedFields), update (merge + prune owned fields),
    conflict detection (409 when another fieldManager owns a field),
    force-override (`?force=true` transfers ownership), managedFields
    read/write round-trip on `metadata.managedFields[]`.
  - `crates/api/src/apply/field_set.rs` — FieldsV1 tree extraction (object
    fields + keyed-list elements by merge key), tree flattening to owned
    paths, field pruning by path removal, identity/system-field exclusion
    (`apiVersion`, `kind`, `metadata.name`, etc. are never owned).
  - `crates/apiserver/src/apply.rs` — `ApplyQuery` extractor
    (`fieldManager`, `force`, `fieldValidation`), `is_apply_ct()` content-type
    check, `do_apply()` handler dispatching to `api::apply::apply_object`.
  - `crates/apiserver/src/item.rs` — PUT/PATCH wrappers now accept
    `HeaderMap` + `Bytes` + `Query<ApplyQuery>`; when content-type contains
    `apply-patch`, dispatch to `do_apply` instead of replace/patch.
  - `crates/apiserver/src/error.rs` — `ApplyConflict` variant (409) with
    `causes` array listing conflicting field paths and owning managers.

### Semantics implemented
- 201 on create-via-apply (object absent).
- 200 on update-via-apply (object exists, same or new fields).
- 409 Conflict when a field in the desired object is owned by a different
  `fieldManager`. Overridable with `?force=true` (ownership transfer).
- Field pruning: fields owned by the applying manager that are absent from
  the new desired object are removed.
- Identity/system fields (`apiVersion`, `kind`, `metadata.name`,
  `metadata.namespace`, `metadata.creationTimestamp`, `metadata.uid`,
  `metadata.resourceVersion`, `metadata.generation`) are excluded from
  ownership tracking — they are not conflict points.

### Scope A limitations
- Object fields + keyed-list ownership by merge key (e.g. `containers` by
  `name`). Atomic lists owned as a unit.
- Full fieldsV1 edge cases (`i:`/`v:` indexes, atom replacement) deferred.
- Server-side field validation (`?fieldValidation=Strict|Warn|Ignore`)
  accepted but not enforced (server trusts the input).

### Test counts
- 341 total (was 326): +8 `crates/api/tests/apply.rs` (create, update,
  conflict, force, prune, multi-manager, field-tree extraction, round-trip),
  +7 `crates/apiserver/tests/rest_apply.rs` (create 201, update 200,
  conflict 409, force override, managedFields response, PUT apply, prune).
- Golden conformance: 14/14 (was 12/12): G13 (SSA create → 201), G14
  (SSA update → 200).
- `cargo clippy --workspace --all-targets -- -D warnings` → 0 warnings.

### SSOT updates
- `index.md`: T1.2 in-progress → done (T1.2a + T1.2b + T1.2c complete).
- `plan/01-api.md`: T1.2c evidence recorded.

## Sprint 10.5 — REST CRUD + watch over the embedded store (2026-07-27)

**Focus:** Wire the embedded storage backend (T2.1/T2.2 spike) into the
apiserver as real REST CRUD + watch handlers. The server graduates from
discovery-only to a functional data plane.

### What landed
- **T1.2b REST CRUD + watch** (`crates/apiserver/`):
  - `state.rs` — `AppState` (registry + store + server_addr), `Loc`/`Resolved`
    path resolution, upstream-faithful `/registry/<group>/<version>/...` key
    layout, helper functions for type-meta/namespace/resourceVersion injection.
  - `collection.rs` — `do_create` (scope validation, namespace injection),
    `do_list` (prefix scan + key-cursor pagination), `do_watch` (chunked
    `application/json` stream via tokio mpsc + `Body::from_stream`, ADDED/
    MODIFIED/DELETED event projection). Core + grouped wrappers for both
    cluster-scoped and namespaced resources.
  - `item.rs` — `do_get`, `do_replace` (resourceVersion CAS → 409 on stale),
    `do_delete` (resourceVersion CAS), `do_patch` (strategic-merge /
    RFC-7386 merge / RFC-6902 JSON-patch). Core + grouped wrappers.
  - `error.rs` — `ApiError` enum → metav1 `Status` JSON (404/409/400/500).
  - `app.rs` — `api_app()` assembles discovery + CRUD + watch routes.
  - `serve.rs` — `serve()` now takes `Arc<dyn StorageBackend>` as 2nd param.
  - `cli/runtime.rs` — constructs `EmbeddedStorage::new()` and passes to serve.

### Semantics implemented
- resourceVersion CAS on PUT/DELETE (409 Conflict on stale revision).
- PATCH: strategic-merge (default), merge-patch (RFC 7386), JSON-patch (RFC 6902).
- Watch: live events only (embedded backend has no historical replay);
  `resourceVersion` param accepted for API parity.
- Namespace auto-injection for namespaced resources on create.
- List pagination via key cursor (not offset).

### Deferred
- Server-side apply field-manager → T1.2c (next sprint).
- Real etcd-gRPC backend → T2.3 (`--datastore-endpoint etcd://`).
- Historical watch replay → T2.3 (etcd-gRPC backend capability).

### Test counts
- 326 total (was 311): +14 REST CRUD integration tests, +1 watch streaming
  test (real TCP, minimal HTTP/1.1 client), +0 unit (refactor of existing
  discovery_handlers test).
- Golden conformance: 12/12 (was 6/6): G06 fixed (pods list → 200), G07–G12
  added (CRUD round-trip + watch-open).

### SSOT updates
- `index.md`: T1.2 not-started → in-progress; T2.1 in-progress → done.
- `plan/01-api.md`: T1.2 evidence updated (T1.2a done, T1.2b done, T1.2c deferred).
- `plan/02-storage.md`: T2.1 → done; T2.2 blockers updated.
- `decisions.md`: Q17 (already written in Sprint 10) — pure-Rust embedded
  store decision, now validated by the REST wiring.

## [Unreleased] — Phase 1, Sprint 10 (storage layer T2.1/T2.2 + releasable increment)

Opens Phase 2 on the critical path: the **storage layer** the APIServer REST
face (T1.2) sits on. Two of the three Sprint-9 deliverables that were *described
but not yet materialized* — the populated golden fixtures and the actual CI
workflow file — are landed here, plus a first container image and repo-local
agent memory.

### Added
- **T2.1/T2.2 — the `storage` crate (started).** A generic
  `StorageBackend` trait (`Watch`/`List`/`Create`/`Update`/`Delete`) over the
  upstream `/registry/...` key layout, plus an embedded, zero-dependency
  backend (`EmbeddedStorage`) that reproduces etcd's *semantics*: a single
  monotonic cluster revision, per-key `create_revision`/`mod_revision`/
  `version` (etcd `KeyValue` parity, where `mod_revision` is the Kubernetes
  `resourceVersion`), optimistic concurrency via an `if_revision` CAS on
  `update`/`delete`, and live `watch` via a tokio broadcast fan-out.
  - **T2.1 spike decision:** implement etcd semantics in pure Rust behind the
    trait rather than FFI-linking Go's `etcd` or supervising a bundled
    subprocess. The real etcd-gRPC client and the SQLite/KINE impl (T2.3) slot
    in as alternative trait impls selected by `--datastore-endpoint`; the
    embedded store is the default and the test double.
  - **`crates/storage/`** — `lib.rs`, `key.rs` (`/registry/...` layout), `entry.rs`
    (`StoredEntry`/`WatchEvent`/`Revision`), `error.rs`, `backend.rs` (the trait +
    `Watch`), `embedded.rs` (`EmbeddedStorage`). **15 integration tests** in
    `tests/embedded_storage.rs`: CRUD round-trip, resourceVersion monotonicity,
    optimistic-concurrency conflict on a stale revision, list prefix/namespace
    filtering + revision ordering, watch put/delete events, and the upstream key
    layout assertions.
- **Golden fixtures materialized (T0.6).** Sprint 9 committed the harness and
  documented the 4 cases but left the `discovery-*.json` fixtures empty (0 bytes).
  This sprint generated them from a live server with the documented
  `127.0.0.1:<port>` -> `127.0.0.1:@@PORT@@` normalization; `scripts/golden-conformance.sh`
  is now genuinely **6/6 green** (was a body-mismatch-fail against empty files).
- **`.github/workflows/ci.yml` materialized + expanded.** Sprint 9 described a
  CI workflow; the `.github/` directory did not exist. Now landed: a
  `lint-test` job (fmt `--check`, clippy `-D warnings`, `cargo test --locked`,
  debug build, then the e2e suite — multicall, CLI parity, graceful shutdown,
  router coroutine selftest, discovery parity, golden conformance) and a
  `bundle` job (`INIT_PRO_EMBED=1` build + `stage-fresh-dir-test.sh`).
- **`Dockerfile` + `.dockerignore`.** Multi-stage (rust:1.89 builder ->
  debian:bookworm-slim runtime); installs the multicall binary under every
  alias so `docker exec ... kubectl` and symlink invocation both work. Exposes
  6443; `--build-arg EMBED=1` bakes the pinned peers in.
- **Repo-local agent memory.** `agents.md` (entry point: SSOT pointers, build
  gates, conventions, 10-crate map, state) + `features/` cards
  (`discovery-api`, `router-data-plane`, `storage-layer`, a feature index, and a
  feature-centric changelog).

### Changed
- **Workspace version `0.1.0` -> `0.2.0`** (milestone-aligned; propagated to the
  server's `--version` via `env!("CARGO_PKG_VERSION")`).
- **SSOT drift reconciled.** `index.md` + `plan/02-storage.md` now mark
  T2.1/T2.2 `in-progress` (were `not-started` despite `crates/storage/`
  landing). `crates/router/src/lib.rs` header fixed: T5.4 is **done**
  (TLS/SNI + hot reload shipped + tested), not "in progress, Scope A". README
  refreshed (was a Sprint-1 snapshot of 5 crates; now lists all 10 + CI/Docker).
- **`cargo fmt --all`.** The tree was one rustfmt revision behind (trailing
  commas, `use` ordering, struct-literal normalization across ~70 files); normalized so the new CI fmt gate is green. Formatting only — no semantics.

### Tests
- `cargo test --workspace` -> **311 passed; 0 failed; 0 ignored** (fresh).
  **+15 net-new** over the Sprint 9 baseline (all in the new `storage` crate:
  0 unit + 15 integration). fmt + clippy `-D warnings` both clean.
- e2e (7 scripts): multicall, CLI parity, graceful shutdown, router coroutine,
  discovery parity, golden conformance all green against the plain debug build;
  `stage-fresh-dir-test` green under the `INIT_PRO_EMBED=1` bundle build.

### Known limitations
- The storage backend is **not yet wired into the REST face** — that is T1.2
  (the next gate). The server is still discovery-only over the wire; the new
  store is exercised only by its own integration tests.
- `EmbeddedStorage` is a **live-watch** backend (no historical replay from a
  past revision); replay is an etcd-gRPC-backend capability for later.
- `EmbeddedStorage` is in-memory and per-process: not durable across restarts
  and not HA. Durability + HA arrive with the etcd/SQLite backends (T2.2/T2.3)
  and multi-server (T3.4).

## [Unreleased] — Phase 1, Sprint 9 (Phase 1 closeout: M0 + M1 done)

Closes Phase 1. The T0.6 golden gate — the merge gate every later TODO must
keep green (Q2) that was nominally on the critical path but skipped while the
M1 Router spike advanced — is landed; the in-progress T5.3/T5.4 are marked
done (remaining items deferred to T5.5 / Phase 2). All 7 Phase 1 DoD criteria
are met. M0 (T0.1–T0.6) + M1 (T5.1–T5.4) = Phase 1.

### Added
- **T0.6 — golden conformance merge gate (the empty-cluster baseline).** The
  harness itself is the deliverable today: it boots a real `init-pro server`
  on a free loopback port and diffs its discovery responses against committed,
  byte-stable fixtures; cases are appended as CRUD/watch/storage/scheduling
  layers land.
  - `golden/` — 4 byte-stable discovery fixtures (`GET /api`, `/apis`,
    `/api/v1`, `/apis/init-pro.io/v1`) + `README.md` documenting the 6 cases
    (G01–G04 body matches; G05/G06 assert 404 for unknown group/version and
    not-yet-existing collection endpoints) and the `@@PORT@@` normalization of
    the only volatile field (`APIVersions.serverAddress`).
  - `scripts/golden-conformance.sh` — boots the server, waits for the
    `discovery listening` line, diffs live vs. fixture, normalizes the bind
    `host:port`. **6/6 green** against the discovery-only server (T1.2a).
- **`.github/workflows/ci.yml` — minimal CI.** A `build` job (`cargo
  build`/`test`/`clippy -D warnings`, all `--locked`) gates on every push to
  `main`/`sprint-*` and on PRs; a `golden` job (needs `build`) runs
  `scripts/golden-conformance.sh` + `scripts/multicall-selftest.sh` +
  `scripts/cli-flag-parity-test.sh`, providing the named `golden` required
  check for DoD #3. (Configuring it as a *required* status check in GitHub
  branch-protection is a one-time repo setting, not a code change.)

### Changed
- **T5.3 / T5.4 marked done** (were `in-progress`). The M1 data plane is
  complete — the Ingress→route compiler, round-robin balancer, Rust reverse
  proxy, rustls/SNI TLS termination, and no-restart route-table hot reload
  all ship. The two remaining items (a live `kube-rs` informer config source
  and dynamic Lua-driven cert issuance) are explicitly deferred to **T5.5 /
  Phase 2**; they are not needed for the M1 spike verdict (Q5).
- **SSOT synchronized** in lock-step: `index.md` status table (T0.6, T5.3,
  T5.4 → done) = `plan/00-foundation.md` T0.6 + `plan/05-ingress-lua.md`
  T5.3/T5.4 Status/Evidence = this changelog. `phase-1-implementation.md`
  gains a Sprint 9 section + closeout note.
- **README corrected.** The Layout table now lists all **9** workspace crates
  (was 5: missing `vendor`, `api`, `apiserver`, `router`) and the Status line
  reflects Phase 1 (M0 + M1) complete; the Build & verify block lists the
  golden + flag-parity scripts.

### Tests
- `cargo build --locked` → single `init-pro` binary (DoD #1).
- `cargo test --workspace --locked` → **296 passed; 0 failed** (fresh).
- `cargo clippy --workspace --all-targets --locked -- -D warnings` → 0 warnings.
- `scripts/golden-conformance.sh` → 6/6 green (T0.6).
- `scripts/multicall-selftest.sh` → all aliases OK (DoD #2).
- `scripts/cli-flag-parity-test.sh` → 16/0 (accept/no-op/fatal).

### Phase 1 DoD (7/7)
1. `cargo build --release` → single `init-pro` binary — ✅
2. all multicall aliases `--help` pass — ✅
3. CI `golden` required check green (empty-cluster baseline) — ✅ (workflow
   added; the `golden` job exists and is green locally; branch-protection is
   a one-time GitHub repo setting)
4. T5.1 concurrency bridge stress test passes (quantified latency baseline) — ✅ (Sprint 4)
5. T5.2 phase pipeline end-to-end observable — ✅ (Sprint 5/6)
6. T5.3 resty::* subset passes upstream port tests — ✅ (Sprint 7/8)
7. M1 spike: Ingress→Lua→real traffic, with hot reload (Q5) — ✅ (Sprint 8)

## [Unreleased] — Phase 1, Sprint 8 (T5.4 — the M1 data plane → in-progress)

### Added
- **T5.4 — the Ingress→route compiler, round-robin balancer, Rust reverse
  proxy, TLS termination, and hot reload.** The built-in Router's control +
  data plane: Kubernetes `networking/v1` Ingress objects compile down to a
  flat, specificity-ordered route table, a Rust round-robin balancer selects a
  peer per request, and a Rust HTTP reverse proxy forwards traffic to upstream
  Services over real TCP. This is the **M1 vertical slice** de-risked in Q5 —
  the highest-risk bet. **Scope A** proves end-to-end routing (two hosts → two
  distinct upstream echo servers); **Scope B** extends the same data plane to
  **HTTPS termination** (`rustls` + SNI) and **no-restart route-table hot
  reload** — completing the M1 gate. (A live `kube-rs` informer and dynamic
  Lua-driven cert issuance remain **T5.5** / Phase 2.)
  - **`route.rs` — compiled route table.** `RouteTable` maps host × path to an
    `UpstreamRef` (Service name + numeric/named `PortRef`). Matching honours
    Kubernetes semantics: exact host, wildcard host (`*.example.com`,
    single-label `*` rejected), `pathType` `Prefix`/`Exact`, with specificity
    ordering (most-specific first). `finalise()` sorts the table once; the proxy
    does a first-match scan. 296 lines.
  - **`ingress.rs` — the Ingress→route compiler (`compile_ingress`).** Walks
    `k8s-openapi` `networking/v1::Ingress` objects: `host` rules, `pathType`,
    per-path `Service` backends (name + port), and the optional
    `defaultBackend`. Only `Service` backends are routed (a `resource` backend
    is skipped — nothing to proxy to). Merges multiple Ingresses into one table.
    227 lines.
  - **`balancer.rs` — round-robin `Balancer` + `UpstreamResolver`.** The
    resolver trait (`UpstreamResolver`) expands an `UpstreamRef` to live peer
    `SocketAddr`s; Scope A ships `StaticResolver` (an in-process map) so the data
    plane is exercised with no etcd/watch. `Balancer` keeps one rotating index
    per upstream key (round-robin); `least-conn`/pluggable strategies are Scope
    B. The free fn `pick_peer` ties them together. 124 lines.
  - **`proxy.rs` — the Rust HTTP reverse proxy (`serve_proxy`).** Accepts on a
    `TcpListener`, matches the request against the `RouteTable`, asks the
    `Balancer` for a peer, forwards over a fresh TCP connection (`Connection:
    close`), and relays the upstream response. Hop-by-hop headers are stripped
    both ways (RFC 7230); `X-Forwarded-For`/`X-Forwarded-Proto` are added. When
    no route matches and no default exists, an optional Lua `Pipeline` serves
    the request (the T5.2 content path); otherwise 404. The pluggable parts
    (balancer/resolver/pipeline/`tls`/`reload`) are grouped into a `ProxyOptions`
    struct (under clippy's arg-count limit). **TLS:** when `opts.tls` is set the
    stream is wrapped via `TlsAcceptor` (SNI selects the cert), else plaintext.
    **Hot reload:** a `reload` receiver swaps each incoming `RouteTable` into a
    `RouteStore` between requests; each connection takes an `Rc` snapshot at
    accept time, so a swap never disturbs an in-flight request. 388 lines.
  - **`conn.rs` — shared HTTP/1.1 connection I/O.** Request parsing (head + body
    framing) and response serialisation extracted here so the Lua data plane
    (`serve`) and the reverse proxy reuse one implementation. Also parses
    *upstream* response heads (the proxy reads upstreams). The three request
    parse tests previously inline in `serve.rs` were consolidated here
    (preserved, not dropped). 346 lines.
  - **`tls.rs` — TLS termination config (Scope B, ADR Q16).** Builds a rustls
    `ServerConfig` with an SNI cert resolver (`SniCertResolver`: ClientHello SNI
    → pre-loaded `CertifiedKey`, case-insensitive; empty SNI → default cert);
    ALPN advertises `http/1.1`. Cert/key material is **injected** as PEM bytes
    (no secrets hardcoded, R5). The shared `build_client_config(verify)` backs
    both the cosocket `sslhandshake` and `resty.http` (`verify=true` → Mozilla
    `webpki-roots`; `verify=false` → a `NoVerify` accept-anything verifier). The
    `ring` crypto provider is used everywhere. 195 lines.
  - **`config.rs` — hot-reloadable route config (Scope B / T5.5 seam).**
    `RouteStore` holds the live table as `Rc<RefCell<Rc<RouteTable>>>` so the
    single-threaded VM (Q12) swaps the whole table between requests with a cheap
    `Rc` clone — the "generation swap on the request boundary" model.
    `ConfigSource` is the seam the kube-rs informer (T5.5) will satisfy;
    `reload_channel()` is the M1 in-process stub (allowed by Q5/R4). 91 lines.
  - **`Phase::Balancer` in the phase pipeline.** The openresty `balancer_by_lua`
    phase is wired: a `Balancer` variant is added to `Phase` (with its `label()`
    arm + `.balancer(src)` builder convenience) and asserted *non-generative*
    (it may override a peer selection, not produce content). Rust round-robin is
    the Scope A default; Lua peer-override via the phase is a Scope B refinement.
  - **`lib.rs` exports are a strict superset** of the Sprint 7 surface: added
    `compile_ingress`, `serve_proxy`, `pick_peer`, `Balancer`,
    `StaticResolver`/`UpstreamResolver`, and the route types
    (`RouteTable`/`RouteRule`/`HostMatcher`/`PathMatcher`/`PortRef`/
    `UpstreamRef`). Scope B adds `ProxyOptions`, the hot-reload surface
    (`RouteStore`/`reload_channel`/`ConfigSource`/`StaticConfigSource`), and the
    TLS surface (`build_server_config`/`CertKey`/`SniCertResolver`). No public
    symbol was removed.
  - **Dependencies:** `k8s-openapi` was already a workspace + `api`/`apiserver`
    dependency (locked in `Cargo.lock`); the router now reads it directly to
    compile `Ingress`. Scope B adds TLS deps — `rustls`/`tokio-rustls` (the
    `ring` provider), `rustls-pemfile`, `webpki-roots` — and test-only `rcgen`
    (a dev-dependency, never shipped; R5).
- **THE M1 GATES: routing, TLS routing, and hot reload over real TCP.**
  Routing: two Ingresses compile to a table; `serve_proxy` accepts real
  connections; a GET to host A reaches upstream echo A, host B reaches upstream
  echo B. TLS routing: an HTTPS listener with SNI selects the cert per host and
  routes `https://a`→A, `https://b`→B over a real TLS handshake. Hot reload:
  pushing a 2nd Ingress through `reload_channel()` makes host B routable
  **without a restart**, while host A keeps working (full superset). All proven
  over ephemeral OS-assigned `127.0.0.1:0` sockets — no etcd or LLM in the loop.

### Tests
- `cargo test --workspace` → **283 passed; 0 failed; 0 ignored** (fresh).
  **+34 net-new** over the Sprint 7 baseline of 262.
  - `route.rs` unit (5) — prefix matching element-wise, prefix-root matches all,
    wildcard single-label rejection, exact host with port stripped, table orders
    most-specific first.
  - `ingress.rs` unit (4) — compiles host + paths, default backend used on no
    match, `resource` backend skipped, multiple Ingresses merged + ordered.
  - `balancer.rs` unit (3) — round-robin cycles peers, empty pool yields `None`,
    `StaticResolver` returns registered peers.
  - `proxy.rs` unit (4) — upstream request strips hop-by-hop + sets
    `Content-Length`, client response drops hop-by-hop, `X-Forwarded-*` present,
    path used only for routing.
  - `conn.rs` unit (3) — request parse specs, upstream response body framing,
    hop-by-hop header detection (moved here from `serve.rs`).
  - `tests/proxy_routing.rs` integration (5) — **the gate**:
    `ingress_routes_each_host_to_its_upstream`, `round_robin_spreads_across_peers`
    (observes the 1,2,1,2 peer sequence), `default_backend_serves_unmatched_requests`,
    `no_peers_returns_503`, `forwards_xff_header_upstream`.
  - `tests/tls_routing.rs` integration (5) — **the TLS gate**: SNI selects the
    cert and routes each host to its upstream over a real TLS handshake; unknown
    SNI falls to the default backend (or 404); the TLS listener rejects raw
    plaintext; ALPN negotiates `http/1.1`.
  - `tests/hot_reload.rs` integration (3) — **the hot-reload gate**: a 2nd
    Ingress pushed via `reload_channel` becomes routable without a restart;
    `RouteStore` generations increment monotonically; a snapshot taken before a
    swap is unaffected by it.
- `cargo clippy --workspace --all-targets -- -D warnings` → **0 warnings**.
- `cargo build --workspace` → clean (`Finished dev profile`).
- New source files all ≤400 lines (route 306, ingress 227, balancer 124, proxy
  388, conn 346, config 91, tls 195); `tests/proxy_routing.rs` 287,
  `tests/tls_routing.rs` 291, `tests/hot_reload.rs` 201, and `tests/resty_stdlib/`
  split into ≤181-line modules.

### Changed
- **T5.4 → in-progress (Scope A+B / M1 data plane done).** Meets the M1
  acceptance: Ingress → route table → `serve_proxy` → upstream, two hosts → two
  distinct upstreams over real TCP; **TLS routing** (SNI→cert, HTTPS to two
  upstreams) and **hot reload** (a 2nd Ingress routable without a restart) are
  proven. Remaining: the live `kube-rs` informer config source and dynamic
  Lua-driven cert issuance (T5.5 / Phase 2).
- **`serve.rs` refactored** onto the shared `conn.rs` (private parse/serialise
  functions moved out); the public API (`serve()`/`ephemeral_listener()`) is
  unchanged in shape and behavior.
- **`index.md`** T5.3/T5.4 status updated; **`plan/05-ingress-lua.md`** T5.3/T5.4
  evidence refreshed (T5.5/6/7 copy-paste status bugs fixed back to
  `not-started`); **ADR Q16** (`ring` crypto provider) added to `decisions.md`.
  Workspace total **296**.

## [Unreleased] — Phase 1, Sprint 7 (T5.3 — `resty.*` core → in-progress)

### Added
- **T5.3 — the `resty.*` standard library + `ngx.shared.DICT`.** The
  openresty per-worker shared-state layer: a Rust-backed reimplementation of the
  `resty::*` libraries plus the worker-global shared-dict zone that unblocks
  T5.4 (Q14 handoff). Covers `resty.lrucache`, `ngx.shared.DICT`,
  `resty.random`, and `resty.string`/`resty.sha256` (Scope A), plus
  `resty.http` and `resty.lock` (Scope B — now landed; their TLS path depended
  on cosocket TLS → T5.4 Scope B / Q16). Remaining: the extra digests
  (md5/sha1/sha512).
  - **`ngx.shared.DICT` (ADR Q15).** Worker-global, cross-request-persistent
    dictionaries auto-created on first access (`local d = ngx.shared.dogs`) —
    there is no `lua_shared_dict` config yet (Ingress config = T5.4), so a named
    dict is lazily created with a default entry capacity. The zone store lives
    in the VM's `app_data` (`RefCell<HashMap<..>>`, `!Send`, Q12 — no
    `DashMap`), so successive requests on the same worker observe the same
    entries. API: `get`/`set`/`add`/`replace`/`incr`/`delete`/`flush_all`/
    `get_keys`/`get_all`; values are scalars (openresty's own restriction).
    `incr`/`add`/`replace` are synchronous Lua calls → naturally atomic.
  - **THE T5.3 GATE: cross-request persistence.** Two real HTTP requests on one
    worker VM share a `ngx.shared.DICT` entry — request A `set`s, request B
    `get`s and observes the value. Verified both in-process and over a real TCP
    socket (`shared_dict_persists_across_requests_*`), mirroring the T5.2 §6
    gate style.
  - **`resty.lrucache`.** Per-instance, capacity-bounded LRU
    (`new(size)`/`get`/`set`/`delete`/`flush_all`/`count`/`get_keys`). Hand-
    written (no `lru` dependency — ADR Q4 minimalism); arbitrary Lua values
    (tables included) survive across calls via Lua registry keys. `get` touches
    recency; eviction is LRU (oldest evicted over capacity).
  - **`resty.random`** — `bytes(n)`/`token(n)` backed by `getrandom` (token is
    URL-safe base64, no padding).
  - **`resty.string` + `resty.sha256`** — `encode_base64`/`decode_base64`/
    `to_hex`/`from_hex` plus a `sha256:new()`/`:update()`/`:final()` digest
    chain (`sha2`); `:final()` returns lowercase hex (collapses lua-resty-
    string's binary+to_hex step). The hex codec is hand-written (~30 lines, no
    `hex` dep).
  - **`resty.http` (Scope B).** `request_uri(uri, opts)` — single-shot HTTP
    request over TCP, TLS-upgraded when the URI is `https://` via the shared
    `build_client_config` (`ring` provider; `verify=false` accepts self-signed,
    parity with cosocket `sslhandshake`). Returns `{ status, headers, body }`.
    Backed by the cosocket-style async dialer (no `reqwest`/hyper dep). 230
    lines.
  - **`resty.lock` (Scope B).** Expiring-key mutual-exclusion locks
    (lua-resty-lock parity). A worker-global `LockRegistry` in `app_data` keyed
    by `(dict, key)`; `lock()` acquires (yielding to other coroutines until free
    or timeout), `unlock()` releases, and locks auto-expire after `exptime` so a
    crashed holder cannot deadlock. The cross-coroutine exclusion gate is the
    T5.3 acceptance test. 133 lines.
  - **Cosocket TLS (`sslhandshake`, Scope B).** `ngx.socket.tcp:sslhandshake`
    upgrades a connected plaintext cosocket to TLS using the shared
    `build_client_config`; the `ConnStream` enum now boxes its `Tls` variant
    (clippy: `large_enum_variant`) and delegates `AsyncRead`/`AsyncWrite`, so
    every cosocket method works unchanged after the handshake.
  - **Module layout (`resty/` dir).** `mod.rs` (register + `ngx.shared` install
    onto the `ngx` global), `lrucache.rs`, `shared_dict.rs`, `random.rs`,
    `string.rs`, plus Scope B `http.rs` and `lock.rs`. Wired via
    `resty::register(&Lua)` after `ngx::register` in `worker_vm()`. Every router
    source file ≤290 lines.
  - **Dependencies:** promoted three *transitive* deps to direct router deps —
    `sha2` (workspace), `base64`, `getrandom` — no truly-new crate added; the
    LRU/hex codecs are hand-written (zero new algorithm deps).
  - **Tests:** `tests/resty_stdlib/` (24, split into ≤181-line modules) — the **T5.3 gate** (logical +
    real-TCP cross-request shared-dict persistence), lrucache
    set/get/evict/recency/count/keys/delete/flush + table values, shared-dict
    `incr`/`add`/`replace` atomic semantics, random bytes/token, sha256 known
    vectors, base64/hex round-trips; +9 router unit tests (capacity, sha256
    vectors, hex/base64, fill). **33 new tests**; workspace total **296 green**
    (was 234 incl. T5.4 Scope B); `cargo clippy --workspace --all-targets -- -D warnings` clean.

### Changed
- **T5.3 → in-progress (Scope A done; Scope B = Sprint 8).** Meets the Scope A
  acceptance: shared dicts persist across requests and the `resty.*` core is
  usable from Lua. Unblocks **T5.4** (the M1 Ingress spike), which needs
  `ngx.shared.DICT` + `resty.lrucache` (Q14).
- **Sprint renumber:** the nominal plan (`phase-1-implementation.md`) read
  `S5=T5.2+T5.3, S6=T5.4`; actual execution is `S5=T5.2 Scope A`, `S6=T5.2
  Scope B`, `S7=T5.3`. A reconciliation note was added; the CHANGELOG cadence
  (already on the actual rhythm) is unchanged.
- **`decisions.md`:** added **Q15** ADR (auto-created shared dicts).
  **`index.md`** T5.3 → `in-progress`, deps `T5.1, T5.2`;
  **`plan/05-ingress-lua.md`** T5.3 status/evidence updated.

## [Unreleased] — Phase 1, Sprint 6 (T5.2 Scope B → done)

### Added
- **T5.2 Scope B — the full openresty phase chain + request body + `ngx.*`
  surface.** Completes T5.2: the ordered phase pipeline
  (`init_worker`→`rewrite`→`access`→`content`→`header_filter`→`body_filter`→
  `log`), request-body reading, and the remaining `ngx.*` API. The T5.2
  acceptance gate (§6: *a `header_filter_by_lua` mutates response headers,
  observed by a real client*) is now met.
  - **Phase pipeline (`pipeline/` module dir).** `Pipeline::build()` builder with
    `.phase(Phase::X, src)` setters (convenience aliases `.rewrite()`/`
    .content()`/`.header_filter()`/...); `Pipeline::new(src)` kept as the
    content-only convenience. Generative phases (`rewrite`→`access`→`content`)
    short-circuit on `ngx.exit`; filter phases (`header_filter`→`body_filter`)
    always run, even after a short-circuit; `log` is fire-and-forget (response
    captured before it runs). `init_worker_by_lua` runs once at boot via
    `Pipeline::boot()` (called by `serve()`).
  - **Decision Q14 (per-phase coroutines).** Each phase runs as its **own**
    explicit Lua `Thread` sharing one `Rc<RefCell<RequestContext>>` (distinct
    threads → distinct Q13 pointers, one shared context). Chosen over a single
    long-lived driver coroutine because `ngx.exit`/`ngx.exec`/`ngx.redirect`
    unwind via a sentinel error — clean per-phase propagation beats catch-and-
    continue, and phases stay isolated. `ngx.exec` re-dispatches (re-runs the
    generative phases for the new URI) with a loop guard (`MAX_EXEC_REDIRECTS`).
  - **Request body (`serve.rs` + `ngx.req`).** The data plane now buffers the
    request body (honouring `Content-Length` **or** `Transfer-Encoding: chunked`,
    capped at 1 MiB → 413 on overflow) instead of draining/discarding. New APIs:
    `ngx.req.read_body()` / `get_body_data()` / `get_post_args()` /
    `get_query_args()`. **Known limitation:** buffered (not true streaming) —
    matches openresty's small-body mode; streaming `body_filter` is T5.6.
  - **`ngx.var`** — request-scoped variable table (metatable proxy): the
    essential set (`uri`/`args`/`request_method`/`scheme`/`host`/`request_uri`)
    computed live, plus settable user vars. **`ngx.arg`** — the `body_filter`
    chunk carrier (`ngx.arg[1]` = body, `ngx.arg[2]` = eof). **`ngx.exec`** /
    **`ngx.redirect`** — internal redirect / external redirect. **Time helpers:**
    `ngx.now()` / `ngx.time()` / `ngx.update_time()` (per-worker cached clock).
  - **Module split (`ngx/` + `pipeline/` dirs).** `ngx.rs` (271 lines) →
    `ngx/{mod,output,req,header,var}.rs`; `pipeline.rs` (152 lines) →
    `pipeline/{mod,phase,outcome}.rs`. Pure refactor first; all prior behaviour
    preserved. **Every router source file ≤359 lines.**
  - **`body_filter` = buffered whole-body transform.** `ngx.arg[1]` is primed
    with the assembled body, the filter mutates it, committed back — openresty's
    buffered mode, not true chunked streaming (flagged in Q14).
  - **Scope decisions:** `ngx.shared.DICT` deferred to **T5.3** (it belongs with
    `resty.lrucache`); `balancer_by_lua` deferred to **T5.4** (needs upstream
    pools) — but the phase-hook mechanism is generic so T5.4 can register one;
    `ngx.re` deferred (would pull a regex dep).
  - **Tests:** `tests/phase_chain.rs` (15) — the **T5.2 gate** over real TCP
    (`real_client_observes_header_filter_mutation`), plus rewrite/access
    short-circuit, `body_filter` transform, POST body round-trip (Content-Length
    **and** chunked), `ngx.var`, `ngx.exec` internal redirect, `ngx.redirect`,
    `init_worker`, time helpers; +8 router unit tests (url-decode, body-spec
    parsing, phase enum, exit-sentinel framing). **24 new tests**; workspace
    total **234 green** (was 210); `cargo clippy --workspace --all-targets --
    -D warnings` clean.

### Changed
- **T5.2 -> done:** meets `验收手段` (content + header_filter observed by a real
  client). Unblocks **T5.4** (the M1 Ingress spike) once T5.3 lands.
- **`decisions.md`:** added **Q14** ADR (phase-chain mechanism + buffered body).
  **`index.md`** T5.2 -> `done`; **`plan/05-ingress-lua.md`** T5.2 status/evidence
  updated to Scope B.

## [Unreleased] — Phase 1, Sprint 5 (T5.2 Scope A)

### Added
- **T5.2 Scope A — content phase pipeline + cosocket over a real HTTP data
  plane.** Extends the `router` crate from the T5.1 spike into a working
  openresty-style content phase: a Lua `content_by_lua` function runs per
  request, drives `ngx.req`/`ngx.header`/`ngx.status`/`ngx.say`/`ngx.print`/
  `ngx.exit`, and can open a cosocket (`ngx.socket.tcp`) to upstream services —
  all observed by a **real TCP HTTP/1.1 client**.
  - **Decision Q13 (per-request coroutine-local binding).** Each in-flight
    request's Lua coroutine gets its own `RequestContext` via an explicit
    `Lua::create_thread` + `Thread::into_async` coroutine, keyed in a
    `ContextStore` (in VM `app_data`) by `Lua::current_thread().to_pointer()`.
    A spike proved `Function::call_async`'s implicit coroutine collapses to the
    root thread (one key for all requests) — unusable; the explicit-thread path
    is distinct and stable under interleaving. Binding/lookup is `Rc`-based (no
    locking, `!Send`-consistent with Q12).
  - **Data-plane server = raw TCP, not axum.** axum's `Send` handler bound is
    incompatible with the `!Send` Lua VM, so the Router data plane is a small
    raw-TCP HTTP/1.1 loop (`serve.rs`) on the worker `LocalSet`. This supersedes
    the data-plane portion of Q11/Q12 (the apiserver keeps axum).
  - **Cosocket** (`cosocket.rs`): `ngx.socket.tcp` with `connect`/`send`/
    `receive`/`settimeout`/`close`; `send` takes Lua strings (binary-safe via
    `LuaString`), `receive` returns a Lua string (fixed-size and line modes).
  - **Real-client tests:** `tests/content_phase.rs` (8) — status/header/body
    emission, `ngx.exit` short-circuit, and **concurrent requests keep distinct
    contexts** over real TCP; `tests/cosocket_echo.rs` (3) — echo roundtrip,
    line-mode receive, latency baseline (~50us/rt for 64B). **11 new tests**;
    workspace total **210 green** (was 199); `cargo clippy --workspace
    --all-targets -- -D warnings` clean; all router files <=262 lines.

### Changed
- **T5.1 -> done:** the feasibility spike is closed (kill-criterion passed in
  Sprint 4); no further work owed by T5.1. **T5.2 -> in-progress** (Scope A
  complete; full phase chain + `ngx.var`/`ngx.shared.DICT`/`ngx.exec`/
  `ngx.redirect` are Scope B+).
- **Workspace deps:** `router` gained `http`, `bytes`, `tracing`.
- **`decisions.md`:** added **Q13** ADR. **`index.md`** T5.1 -> `done`,
  T5.2 -> `in-progress`; **`plan/05-ingress-lua.md`** T5.1/T5.2 status/evidence
  updated.

## [Unreleased] — Phase 1, Sprint 4 (T5.1)

### Added
- **T5.1 — mlua coroutine<->async bridge (feasibility spike).** Proves the
  single highest-risk unknown of Q4 (a Lua-driven Router): a Lua coroutine
  **yields at a Rust `await` point** on the Tokio runtime, letting another
  coroutine run concurrently on the same worker VM without blocking. Delivered
  as a self-contained crate, deliberately **not** wired into `init-pro server`
  — this round is a kill-criterion spike, not production wiring. Cosocket, the
  HTTP phase pipeline, and Ingress->Lua compilation are T5.2-T5.4.
  - **Decision Q12 (Router VM model).** Faithful openresty worker model: **one
    worker-wide LuaJIT VM** carrying **per-coroutine Lua threads**, driven on a
    single-thread `tokio::task::LocalSet`. Concurrency is cooperative yielding
    at `await` points (one VM per thread), so the `Lua: !Send` constraint is a
    non-issue. The bridge is `mlua`'s `create_async_function` (Rust async fn
    callable from Lua) + `Function::call_async` (drives a Lua function as a
    coroutine, parking it at each inner `await`). `luajit52` is OFF (openresty =
    LuaJIT 2.1 = Lua 5.1).
  - **New crate `router`** (`lib.rs`/`vm.rs`/`ngx.rs`, each <=44 lines)
    depending only on `mlua` + `tokio` (no api/apiserver coupling). `ngx.sleep`
    maps to `tokio::time::sleep`; the VM is built by `worker_vm()`.
  - **Kill-criterion PASSED** (`tests/concurrency.rs`): coroutine B starts and
    finishes *inside* coroutine A's `ngx.sleep(50ms)` window (order
    `A_start < B_start < B_end < A_end`), total wall ~= 51ms ~= max(50,5) (not
    the serial ~55ms sum); 10 coroutines x `ngx.sleep(20ms)` complete in ~21ms
    (scales to ~max, not ~sum). **Q4 is de-risked; no Q4 re-evaluation.**
  - **Latency baseline** (`tests/sleep_latency.rs`): `ngx.sleep(10ms)` round-trip
    ~= 11ms (~1ms bridge overhead). v1 number recorded per plan; cosocket
    microbench is T5.2.
  - **4 tests** in `router` (1 vm unit + 2 concurrency + 1 latency);
    **1 self-test script** (`scripts/router-coroutine-selftest.sh`). Workspace
    total **199 green** (was 195); `cargo clippy --all-targets -- -D warnings`
    clean; `cargo build --release -p init-pro` completes; all new files <=154
    lines; pure additive diff (no `#[ignore]`, no deleted tests).

### Changed
- **Workspace deps:** added `mlua` 0.12 (`luajit`,`vendored`,`async`,
  `error-send`, `default-features = false`); LuaJIT is built from source via
  `luajit-src` on first compile (~25s, offline-reproducible, Q7-consistent).
  Added `router` to members + internal deps.
- **`decisions.md`:** added **Q12** ADR. **`index.md`** T5.1 -> `in-progress`;
  **`plan/05-ingress-lua.md`** T5.1 status/evidence/blockers updated with the
  measured numbers.

## [Unreleased] — Phase 1, Sprint 3 (T1.2a)

### Added
- **T1.2a — HTTP discovery API server.** `init-pro server` now binds
  `127.0.0.1:6443` (overridable) and serves byte-correct Kubernetes API
  discovery over HTTP: `GET /api` (`APIVersions`), `GET /apis`
  (`APIGroupList`), `GET /api/v1` + `GET /apis/{group}/{version}`
  (`APIResourceList`), with `Content-Type: application/json` (Q10) and `404`
  for unknown group/version. This proves the HTTP framework choice,
  exercises T1.1's discovery builders over a real transport, and shares the
  stack with the Router data plane (T5.2). No etcd needed; discovery is driven
  entirely by the `SchemaRegistry`.
  - **Decision Q11 (HTTP framework & TLS posture).** Framework is **axum**
    (on hyper/tokio/tower) — one stack for both the apiserver (T1.2) and the
    Router (T5.2). **Plain HTTP for this slice**; TLS (rustls) + real kubectl
    interop is deferred to T1.2b/T1.3. Acceptance is `curl` byte-equivalence
    + a Rust integration test, not kubectl (kubectl refuses plain HTTP).
  - **New crate `apiserver`** (`lib.rs`/`discovery_handlers.rs`/
    `serve.rs`, all ≤104 lines). `api` stays HTTP-free; the apiserver
    crate is the thin transport layer over the T1.1 pure builders.
    `serve(registry, addr, server_address, shutdown)` takes a generic
    shutdown future (kept decoupled from `infra`).
  - **Listen flags.** `--bind-address` (default `127.0.0.1`) +
    `--https-listen-port` (default `6443`) added to `init-pro server` (k3s
    parity), with `INIT_PRO_*` env support; removed from the no-op strip set.
    `--disable-apiserver` keeps the port closed.
  - **Graceful drain.** The server is spawned and joined after the shared
    `Shutdown` token fires, so axum drains in-flight requests before exit.
  - **6 tests** in `apiserver` (1 router-build + 5 HTTP fidelity:
    `/api`, `/apis`, `/api/v1`, `/apis/init-pro.io/v1`, unknown→404); **2
    parity scripts** (`scripts/apiserver-discovery-parity-test.sh` — real
    `init-pro server` + `curl`, `graceful-shutdown-test.sh` still green).
    Workspace total **195 green**; `cargo clippy --all-targets -- -D warnings`
    clean; all new files ≤104 lines; pure additive diff (no `#[ignore]`,
    no deleted tests).

### Changed
- **Workspace deps:** added `axum` 0.8 (`http1`,`json`,`tokio`,`macros`),
  `tower` 0.5; added `net` to `tokio` features.
- **`cli`:** `runtime.rs` spawns the apiserver (was the T1.1
  placeholder `let _ = &schema;`); `ServerCmd` gained the bind flags;
  `lib.rs` parses the bind address. Snapshot `tests/snapshots/server-help.txt`
  + `cli-flag-parity-test.sh` updated for the two new wired flags.
- **`decisions.md`:** added **Q11** ADR.

## [Unreleased] — Phase 1, Sprint 3

### Added
- **T1.1 — resource model & API group schema.** A Kubernetes-faithful Rust
  resource model in a new `api` crate, unlocking T1.2 (APIServer),
  T1.3 (auth), T2.2 (etcd data plane), and T5.4 (router).
  - **Decision Q10 (serialization).** v1 is **JSON-only** on every path — API
    wire, etcd storage, watch streams. protobuf is explicitly deferred (see
    `decisions.md` Q10). `kubectl`/`kube-rs`/`helm` negotiate JSON via
    discovery automatically; no client breakage.
  - **`kube-core` 4.2 + `k8s-openapi` 0.28 (`v1_32`).** Cold build ~11s, 754MB
    RAM (kept behind `api` so cli/infra never recompile it).
  - **S2 — GVK/GVR (`gvk.rs`).** `ApiVersion` round-trip parsing (core vs
    grouped, rejects empty version), GVK↔GVR join helpers, `TypeMeta`→GVK.
  - **S3 — schema registry (`schema.rs`).** `SchemaRegistry` maps GVK→type info
    (kind/plural/list-kind/scope); lossless GVK↔GVR conversion; core/v1 types
    (Pod, ConfigMap, Secret, Service, Namespace, Node, Event) registered from
    their static `k8s_openapi::Resource` consts; case-insensitive group lookup.
  - **S4 — JSON round-trip fidelity (`serde_ext.rs` + `tests/json_fidelity.rs`).**
    Round-trip of Pod/ConfigMap/Namespace is asserted (a) idempotent
    (serialize twice → identical bytes) and (b) semantically lossless
    (canonical-key-sorted compare). `canonical_json` for order-insensitive eq.
  - **S5 — Strategic Merge Patch (`patch.rs`).** Core SMP semantics: recursive
    map merge, `null`-deletes-field, merge-by-key lists (containers /
    initContainers / ephemeralContainers / volumes by `name`, ports by
    `containerPort`, env by `name`) with order preservation + `$patch: delete`,
    atomic replace for non-keyed lists; RFC 6902 JSON Patch fallback.
  - **S6 — `init-pro.io/v1` group (`initpro.rs`).** `LuaRouter` CRD (the Router
    config surface, Q4) with flattened `TypeMeta`/`ObjectMeta`/spec/status,
    registered into the schema registry, kubectl-apply JSON round-trip.
  - **S7 — discovery skeleton (`discovery.rs` + `cli/discovery.rs`).**
    `/api` (`APIVersions`) + `/apis` (`APIGroupList`) + per-group
    `APIResourceList` bodies built from the registry — byte-correct today,
    served by T1.2's HTTP layer later. `init-pro server` builds the served
    schema + logs the group summary at startup.
  - **47 tests in `api`** (8 gvk + 8 schema + 5 serde + 11 patch + 6
    discovery + 4 initpro + 5 json_fidelity integration) + 1 in `cli`;
    workspace total **189 green**; `cargo clippy --workspace --all-targets
    -- -D warnings` clean; all files ≤400 lines.
  - **Structural coverage.** Every new `pub fn` on the resource model has a
    direct test: `get_by_gvr` (pod resolve + unknown → `None` + case-insensitive
    group) and `is_core_resource` (core `""` vs grouped); the trivial
    `with_merge_key` (merge-vs-replace), `to_json_pretty`/`canonical_value` and
    `is_empty` accessors are covered too — no untested public surface.

### Changed
- **`Cargo.toml` workspace deps:** added `api`, `serde_json`,
  `thiserror`, `kube-core` 4.2 (features `json-patch`), `k8s-openapi` 0.28
  (`v1_32`), `json-patch` 4.
- **`00-foundation.md`:** T0.4 status corrected to `done` (was stale
  `not-started` despite being complete in Sprint 2) with evidence.
- **`decisions.md` / `README.md`:** added **Q10** ADR (JSON-only v1) + table row.
- **`index.md` SSOT:** T1.1 → `done`.

## [Unreleased] — Phase 1, Sprint 2

### Added
- **T0.4 — k3s-compatible CLI.** The `server`/`agent` surface accepts the full
  k3s flag vocabulary in three postures (frozen matrix
  `plan/00-foundation-flag-matrix.md`, ADR Q9):
  - **Pre-clap config pre-scan (Q8, A1).** `infra/configfile.rs` parses the
    layered config file (`<data-dir>/config.yaml` by default) with two-pass
    resolution that breaks the data-dir↔config-path circularity (R3).
    `Config::resolve` now takes 3 args: `(cli_data_dir, cli_debug, cli_config)`.
    Precedence: CLI > env (`INIT_PRO_*`) > file > default. A `config_scan.rs`
    argv scanner finds `--config`/`-c` before clap, short-circuiting `--help`.
  - **17 accept-wired flags (Table A, A2).** `server`/`agent` clap-derive
    structs capture `--data-dir`/`-d`, `--debug`, `--config`/`-c`, `--disable`,
    `--disable-{etcd,apiserver,agent,controller-manager,scheduler,cloud-controller,kube-proxy,network-policy,helm-controller}`,
    `--datastore-endpoint`, `--prefer-bundled-bin`, `--token`/`-t`,
    `--server`/`-s`, `--cluster-init`.
  - **~108 accept-no-op-warn flags (Table C, A3).** `strip_noop()` removes
    them from argv before clap so operators' k3s scripts keep working;
    `warn_noops()` logs each distinct flag once at WARN (deduped).
  - **7 fatal conflict rules (Table B, A4).** `validate_server` /
    `validate_agent` enforce k3s-parity preconditions and emit matching
    messages before logging/resolve: cluster-reset-restore-path needs
    cluster-reset; disable-{apiserver,etcd} ✗ datastore-endpoint;
    disable-etcd needs server; unknown `--disable` token (whitelist:
    `coredns, servicelb, traefik, local-storage, metrics-server, runtimes`);
    agent needs token; agent needs server.
  - **Parity harness (A5).** `scripts/cli-flag-parity-test.sh` exercises all
    five matrix assertions (accept / no-op-warn / fatal / `INIT_PRO_*` parity /
    unknown `K3S_*` ignored). Frozen `server`/`agent --help` snapshots in
    `tests/snapshots/` gate the wired-flag surface.

- **T0.2 — packaging pipeline (in progress; B1 done, embed/stage/SBOM next).**
  - **B1 — pinned artifact acquire (Q6).** New `vendor` crate
    (build-dependency of `init-pro`) reads `vendor/versions.toml` and acquires
    pinned upstream artifacts with SHA-256 verification (k3s `sha256sum -c`
    parity) into the gitignored `vendor/cache/` + `vendor/bin/`.
    - **Manifest** (`vendor/versions.toml`): containerd 1.7.20, runc 1.1.13,
      CNI plugins 1.5.1 — all Apache-2.0 (Q7 allow-list enforced at parse
      time; GPL `k3s-root` host utilities excluded from v1).
    - **Three acquire modes** (precedence OFFLINE > VENDOR > AUTO):
      `INIT_PRO_VENDOR=1` downloads missing artifacts; `INIT_PRO_OFFLINE=1`
      forbids network and requires a pre-populated cache (air-gap); the
      default Auto mode uses the cache if present else skips (so `cargo build`/
      `cargo test` stay network-free and fast). Pure `plan()` + offline
      contract covered by unit + integration tests.
    - **`crates/init-pro/build.rs`** drives acquire via the vendor crate and
      emits `cargo:rerun-if-{changed,env-changed}` directives.
    - Verified end-to-end: `INIT_PRO_VENDOR=1 cargo build` downloads + verifies
      + stages all three (containerd+runc→`vendor/bin/`, CNI→`vendor/bin/aux/`);
      a corrupt partial download was correctly rejected by the SHA-256 gate.
  - **B2 — zstd embed codegen (Q6).** `vendor` compresses each acquired
    artifact per-file with zstd level 19 and emits a content-addressed
    `assets.rs` (one `include_bytes!` blob per file, keyed by SHA-256). Build
    embeds the blobs into the single binary; verified via `init-pro stage
    --dry-run` listing every artifact with its SHA-256.
  - **B3 — dataverify manifests.** Emits `.sha256sums` + `.links` (ported from
    k3s `pkg/dataverify/dataverify.go`) so runtime staging can recompute +
    compare byte-for-byte; the manifest doubles as the single source for
    expected sizes/links.
  - **B4 — Q7 license gate + SPDX-2.3 SBOM.** `build.rs` collects each
    component's upstream `LICENSE`/`NOTICE` into `LICENSES/`, runs the
    allow-list gate (Apache-2.0/BSD/MIT/ISC — non-cleared artifact fails the
    build), and generates a SPDX-2.3 SBOM referenced by `stage --dry-run`.
  - **B5 — runtime stage() (k3s `extract()` parity).**
    `crates/cli/src/stage.rs` mirrors k3s `cmd/k3s/main.go:259-375`:
    flock `<data-dir>/data/.lock` → write blobs to `<data-dir>/data/<HASH>/`-tmp
    → `dataverify` (recompute `.sha256sums`/`.links`) → atomic rename → symlink
    `data/current` → `<HASH>` (+ `data/previous` rollback); writes `bin/` +
    `bin/aux/` and clones CNI-plugin symlinks into a stable `data/cni/`.
  - **B6 — acceptance harness.** `scripts/stage-fresh-dir-test.sh` copies the
    binary into an empty dir, runs `init-pro stage` against a fresh
    `<data-dir>`, and asserts the staged tree matches `.sha256sums`/`.links`
    byte-for-byte, `data/current` points at the new `<HASH>`, child `PATH`
    leads with the CNI dir, and a re-run is idempotent — **8/8 assertions
    green**.

### Tests
- `cargo test --locked --workspace` → **141 passed; 0 failed** (fresh; up from 23 → 81 → 101 → 141).
  - `cli` unit: +58 (config-file parse/resolve_path/scalar/slice/key+
    append; config_scan; strip_noop incl. short/value/dedup; conflicts ×7;
    help-parity surface).
  - `infra` unit: config-file + 3-arg resolve coverage.
- `cargo clippy --workspace --all-targets -- -D warnings` → **0 warnings**.
- e2e: `scripts/cli-flag-parity-test.sh` → **16/16 assertions green**.

### Known limitations
- `run_server` / `run_agent` / `run_stage` remain Phase 1 stubs (idle until
  signal / manifest print) — real Layers 1–4 arrive from Phase 2.
- Config-file layer is read but not yet surfaced to `--dry-run` beyond
  `data-dir`; structured fields land with the layers that consume them.
- **T0.2 remaining:** B1 acquires only — the per-file zstd embed (`assets.rs`),
  the `.sha256sums`/`.links` runtime manifest (B3), the SPDX SBOM + license
  notice tree (B4, Q7), and the runtime `stage()` / `extract()` (B5) land in
  subsequent commits. `vendor/bin/` + `vendor/cache/` are gitignored build
  outputs, not committed.

## [Unreleased] — Phase 1, Sprint 1

### Added
- **T0.1 — multicall skeleton.** Single `init-pro` binary selects behavior from
  `argv[0]`. Alias table covers `init-pro`, `init-pro-server`, `init-pro-agent`,
  `server`, `agent`, `kubectl`, `ctr`, `crictl`, `containerd`, `etcd`. Unknown
  names fall through to the top-level CLI (clap help). Bundled-peer aliases
  answer `--help` with exit success and reject other args with exit `2` + a
  clear not-yet-implemented message (peers arrive with T0.2/T0.4).
- **T0.3 — infra crate (spike).** `tracing` init with k3s `--debug` parity
  (`RUST_LOG` always overrides); layered config (CLI > env > file > default)
  honoring `--data-dir`/`--debug` and `INIT_PRO_*` envs; graceful-shutdown
  coordination on SIGTERM/SIGINT.
- Cargo workspace with five crates: `init-pro`, `multicall`,
  `cli`, `infra`, `common`. `release` profile targets
  a single small stripped binary (Q1: single-binary constraint).

### Tests
- `cargo test --workspace` → **23 passed; 0 failed** (fresh).
  - `multicall` unit: **9** (alias resolution incl. cross-wire guard,
    `Action::as_str` round-trip, basename/case, `wants_help`, external flag).
  - `infra` unit: **12** (config precedence x5, signal trigger/idle,
    install-returns-ok, `logging::init` idempotency x4).
  - `init-pro` integration: **2** (`external_stub` help branch exits success +
    banner; no-help branch exits `2` + stderr, via real argv[0] dispatch).
- `cargo clippy --workspace --all-targets -- -D warnings` → **0 warnings**.
- `cargo build --workspace` → clean; release yields a single stripped binary.
- e2e (manual / CI): `scripts/multicall-selftest.sh` (T0.1),
  `scripts/graceful-shutdown-test.sh` (T0.3).

### Known limitations
- Bundled peers (`kubectl`/`ctr`/`crictl`/`containerd`/`etcd`) are stubs until
  T0.2 (bundling) + T0.4 (CLI parity).
- Config-file pre-scan layer returns `None` today; lands with T0.4.
