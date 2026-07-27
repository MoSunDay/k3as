# Layer 5 — 内置 Router (openresty Rust port)

Mirrors `index.md` TODO IDs **T5.1–T5.7**.

> **Repositioned per Q4/Q5.** This is **not** an "Ingress addon." It is the
> **built-in Router**: a first-class internal component — a Rust reimagining
> of openresty whose data plane is driven by **Lua**. Ingress objects
> compile to Lua routes (T5.4). The Router is also exposed as a **platform
> configuration variable** (T5.7) readable by node/scheduling/netpol code.
>
> This layer is the **first vertical slice (M1)** after foundation: the
> highest-risk bet gets de-risked first (Q5).

Reference: openresty phase model (`init_by_lua`, `init_worker_by_lua`,
`rewrite_by_lua`, `access_by_lua`, `content_by_lua`, `header_filter_by_lua`,
`body_filter_by_lua`, `log_by_lua`, `balancer_by_lua`) and the
`resty::*` library ecosystem — see `link_repos/openresty/`.

---

## T5.1 — mlua + coroutine↔async 桥

- **目标 / Goal**
  Run Lua (LuaJIT semantics via `mlua`'s LuaJIT/Lua5.1 backend) inside a
  Tokio runtime, with Lua coroutines resuming on `await` points — the
  technical crux of the whole layer.

- **核心实现 / Core implementation**
  - `mlua` with LuaJIT build; yield/resume bridged to async via a
    driver that parks the Lua coroutine on a Rust future.
  - Yield primitives the standard phases use: `ngx.sleep`, `ngx.location
    .capture`, cosocket `receive`/`send` (all map to async I/O).
  - Per-request Lua VM isolation vs worker-wide VM — decide + document.

- **验收手段 / Acceptance**
  - Unit test: a Lua coroutine does `ngx.sleep(0.01)` between two prints;
    assert ordering and that the worker stays responsive (another request
    served concurrently).
  - Microbench: cosocket echo at parity-ish latency vs openresty baseline.

- **状态 / Status** — done (coroutine<->async bridge PROVEN; spike closed.
    cosocket / HTTP phase pipeline / Ingress compilation are T5.2-T5.4)
- **证据 / Evidence** — new crate `router` (`lib.rs`/`vm.rs`/`ngx.rs`,
    each <=44 lines; depends only on `mlua` + `tokio`). **Kill-criterion PASSED**
    (`tests/concurrency.rs`): coroutine B starts and finishes *inside* coroutine
    A's `ngx.sleep(50ms)` window (order `A_start < B_start < B_end < A_end`),
    total wall ~= max(50,5)=51ms (not the serial ~55ms sum); 10 coroutines x
    `ngx.sleep(20ms)` complete in ~21ms (scales to ~max, not ~sum). Latency
    baseline (`tests/sleep_latency.rs`): `ngx.sleep(10ms)` round-trip ~= 11ms.
    VM model + bridge mechanism documented in ADR **Q12**. Workspace total
    **199 green** (195 + 4 new); `cargo clippy --all-targets -- -D warnings`
    clean; `scripts/router-coroutine-selftest.sh` green. Not yet wired into
    `init-pro server` (kept an independent spike).
- **卡点 / Blockers** — none. The Q4 make-or-break is resolved: the coroutine
    bridge is real and non-blocking (no Q4 re-evaluation triggered). Remaining
    scope (cosocket, phase hooks, `resty::*`, Ingress->Lua) is T5.2/T5.3/T5.4.
- **依赖 / Depends on** — T0.3

---

## T5.2 — HTTP 管线 + phase hooks

- **目标 / Goal**
  An HTTP request pipeline invoking the openresty phase hooks in order,
  with `ngx.*` API surface available in each phase.

- **核心实现 / Core implementation**
  - Phases: `init` (worker boot), `init_worker`, per-request
    `ssl_certificate`→`rewrite`→`access`→`content`→`header_filter`→
    `body_filter`→`log`; `balancer` for upstream selection.
  - `ngx.*` API (`ngx.var`, `ngx.req`, `ngx.header`, `ngx.exit`,
    `ngx.exec`, `ngx.redirect`, `ngx.shared.DICT`) implemented against
    the live request.
  - `hyper` body streaming; phase return values steer the pipeline
    (`ngx.exit(STATUS)`).

- **验收手段 / Acceptance**
  - Integration: a Lua `content_by_lua` writes a body + status; a
    `header_filter_by_lua` mutates headers; both observed by a real client.

- **状态 / Status** — done (Scope A + Scope B complete).
- **证据 / Evidence** — **Scope A** (Sprint 5): content-phase pipeline +
    cosocket over a raw-TCP HTTP/1.1 data plane; coroutine-local per-request
    binding (**ADR Q13**) keeps N concurrent requests distinct. **Scope B**
    (Sprint 6): the full ordered phase chain (`init_worker`→`rewrite`→
    `access`→`content`→`header_filter`→`body_filter`→`log`) via
    `Pipeline::build()` (**ADR Q14**: each phase = its own Lua `Thread`
    sharing one `Rc<RefCell<RequestContext>>`; generative phases short-circuit
    on `ngx.exit`, filters always run, `ngx.exec` re-dispatches loop-guarded);
    request-body buffering (`Content-Length` **and** `Transfer-Encoding:
    chunked`, capped 1 MiB → 413) with `ngx.req.read_body`/`get_body_data`/
    `get_post_args`/`get_query_args` (**buffered, not streaming** — flagged in
    Q14); `ngx.var` proxy, `ngx.arg` body-filter carrier, `ngx.exec`/
    `ngx.redirect`, `ngx.now`/`time`/`update_time`. Module split:
    `ngx/{mod,output,req,header,var}.rs` + `pipeline/{mod,phase,outcome}.rs`.
    Real-client tests PASS: `tests/phase_chain.rs` (15) including the §6 gate
    `real_client_observes_header_filter_mutation`, plus `content_phase.rs`
    (8) + `cosocket_echo.rs` (3). Workspace total **234 green**;
    `cargo clippy --workspace --all-targets -- -D warnings` clean; all router
    source files <=359 lines.
- **卡点 / Blockers** — none. axum's `Send` bound is incompatible with the
    `!Send` Lua VM, so the data plane uses raw TCP (supersedes the data-plane
    portion of Q11/Q12; the apiserver keeps axum).
- **依赖 / Depends on** — T5.1

---

## T5.3 — resty::* 等价标准库

- **目标 / Goal**
  A Rust-backed reimplementation of the `resty::*` libraries users expect:
  `resty.core`, `resty.http`, `resty.lrucache`, `resty.random`,
  `resty.lock`, `resty.sha*`, `resty.md5`, etc.

- **核心实现 / Core implementation**
  - Implement as Lua-exposed Rust modules under `router::resty::*`.
  - `resty.http` backed by `reqwest`/hyper client (cosocket-compatible API).
  - `resty.lrucache` backed by Rust LRU; `ngx.shared.DICT` API parity.
  - Compatibility test corpus taken from openresty lua-resty-* test suites
    where license permits.

- **验收手段 / Acceptance**
  - Port a sample of lua-resty-core / lua-resty-http tests; pass rate
    recorded as the v1 baseline.

- **状态 / Status** — done (Scope A+B core: resty.lrucache + ngx.shared.DICT + resty.random + resty.string/sha256 + resty.http + resty.lock + cosocket sslhandshake; remaining digests md5/sha1/sha512 + full upstream lua-resty-* port-test corpus deferred to Phase 2)
- **证据 / Evidence** — Sprint 7/8: `crates/router/src/resty/` (lrucache/shared_dict/random/string/http/lock); `resty::register` wired in `worker_vm()`; cosocket TLS (`sslhandshake`, ring provider — Q16); `tests/resty_stdlib/` (24) incl. the T5.3 gate (request A `set`s into `ngx.shared.DICT`, request B `get`s the value over real TCP — `shared_dict_persists_across_requests_real_tcp`). Workspace 296 green; clippy clean.
- **卡点 / Blockers** — Licensing of upstream test fixtures; vendor only
    what's redistributable.
- **依赖 / Depends on** — T5.1, T5.2

---

## T5.4 — 内置 Router 核心 + Ingress→Lua 路由编译

- **目标 / Goal**
  The Router core: route tables, upstream pools, and a compiler that turns
  Kubernetes `Ingress` (and `HTTPRoute`/`Gateway` later) into Lua route
  programs the data plane executes.

- **核心实现 / Core implementation**
  - Router watches `Ingress`/`IngressClass`/`Secret`(TLS)/`Service`+`Endpoints`
    via informers (kube-rs).
  - Compiler emits a Lua routing table (hosts → paths → upstream + rewrites)
    loaded atomically via T5.5 hot reload.
  - Upstream selection by `balancer_by_lua` (round-robin/least-conn; pluggable).
  - TLS termination via `rustls` + `ssl_certificate_by_lua` SNI hook.
  - Default backend + path types (`Prefix`/`Exact`/`ImplementationSpecific`).

- **验收手段 / Acceptance**
  - **M1 spike acceptance (Q5):** apply an Ingress; `curl` the host → routed
    to the right Service; TLS host works; a second Ingress updates routing
    with no restart.
  - Pinned in T0.6 golden.

- **Scope split (Sprint 8a/8b).** T5.4 is large (5 subsystems from zero);
  split to control risk:
  - **Scope A (Sprint 8a) — HTTP routing + proxy pathway** (DONE):
    Ingress→`RouteTable` compiler, Rust round-robin balancer + upstream
    resolver, Rust HTTP reverse proxy (`serve_proxy`), stub in-process config
    source, `Phase::Balancer` wired. Acceptance: compile Ingress → `curl` host →
    traffic reaches the right upstream echo server → second host routes to a
    different upstream (PROVEN over real TCP).
  - **Scope B (Sprint 8b) — TLS + hot reload**, completes the M1 gate:
    `rustls` + SNI termination (`ssl_certificate_by_lua`), informer/watch
    config source → atomic route-table swap (T5.5).

- **状态 / Status** — done (Scope A+B / M1 data plane: route compiler + round-robin balancer + Rust reverse proxy + rustls/SNI TLS termination + no-restart hot reload; remaining: live kube-rs informer + dynamic Lua cert issuance → T5.5/Phase 2)
- **证据 / Evidence** — Sprint 8: `route.rs` (`RouteTable`, host/wildcard +
  Prefix/Exact path matching, specificity ordering), `ingress.rs`
  (`compile_ingress` over `k8s-openapi` `networking/v1::Ingress`), `balancer.rs`
  (round-robin `Balancer` + `UpstreamResolver`/`StaticResolver`), `proxy.rs`
  (Rust reverse proxy `serve_proxy` w/ `ProxyOptions`: route→balance→forward→
  relay, hop-by-hop stripping, XFF, TLS accept, hot-reload `RouteStore` swap),
  `conn.rs` (shared HTTP/1.1 I/O), `config.rs` (`RouteStore` generation swap +
  `reload_channel` stub), `tls.rs` (`build_server_config` + `SniCertResolver`,
  ring provider — Q16), `Phase::Balancer` in `pipeline/phase.rs`. `tests/`
  proxy_routing (5) + tls_routing (5) + hot_reload (3): the M1 gates — two hosts
  → two distinct upstreams over real TCP (HTTP + HTTPS/SNI), and a 2nd Ingress
  becomes routable without a restart. Workspace 296 green; clippy clean.
- **卡点 / Blockers** — Full no-restart reload needs the live `kube-rs` informer
    config source (T5.5); dynamic Lua-driven cert issuance is deferred.
- **依赖 / Depends on** — T5.2, T5.3, T1.1

---

## T5.5 — 热加载 / 动态配置 (no-restart reload)

- **目标 / Goal**
  Apply route/upstream/TLS changes without dropping the worker or existing
  connections (openresty `lua_load_shared`/HRP reload parity).

- **核心实现 / Core implementation**
  - Generation-stamped routing tables; atomic swap on phase boundary.
  - Shared-dict migrations; long-lived cosockets drained on upstream change.
  - Reload signal path from T5.4 informer → compiler → swap.

- **验收手段 / Acceptance**
  - In-flight request completes with old route while new requests use the
    new route (golden).

- **状态 / Status** — not-started
- **证据 / Evidence** — —
- **卡点 / Blockers** — none
- **依赖 / Depends on** — T5.4

---

## T5.6 — ServiceLB (L4/LB 数据面)

- **目标 / Goal**
  Router-embedded L4 load balancing for `LoadBalancer` Services (complements
  the CNI-level ServiceLB in T4.3 for HTTP-aware / SNI-aware routing).

- **核心实现 / Core implementation**
  - TCP/SNI stream routing in the Router (TLS passthrough by SNI).
  - Reuses upstream pools / health checks from T5.4.
  - Allocates/announces the LB VIP, integrates with T4.3 dataplane.

- **验收手段 / Acceptance**
  - Golden: a TCP Service gets a VIP and load-balances across endpoints.

- **状态 / Status** — not-started
- **证据 / Evidence** — —
- **卡点 / Blockers** — none
- **依赖 / Depends on** — T5.4, T4.3

---

## T5.7 — 内置 Router 作为平台配置变量

- **目标 / Goal**
  Expose the Router's state/Lua context as a **global platform configuration
  variable**: node, scheduler, and netpol code can read Router variables,
  reserving a seam for whole-platform policy extensions.

- **核心实现 / Core implementation**
  - A typed Lua config context (`init-pro.router.var.*`) published from the
    Router to a platform config bus (T0.3).
  - Read API consumed by Layer 3/4 components (e.g., node labels derived
    from Router role vars; scheduling hints; netpol defaults).
  - Versioned schema; changes hot-propagate via T5.5 machinery.

- **验收手段 / Acceptance**
  - Demo: a Lua var set in the Router is observable from a scheduler
    extender (T3.2) and influences placement (golden).

- **状态 / Status** — not-started
- **证据 / Evidence** — —
- **卡点 / Blockers** — Scope of "platform policy extension" — keep v1
    read-only from non-Router code.
- **依赖 / Depends on** — T5.4, T4.5
