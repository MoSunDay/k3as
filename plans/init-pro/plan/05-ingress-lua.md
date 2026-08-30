# Layer 5 — 内置 Router (openrusty h2c 网关)

Mirrors `index.md` TODO IDs **T5.1–T5.7**.

> **Repositioned per Q4/Q5; rewritten per Q32 (Sprint 25).** This is
> **not** an "Ingress addon." It is the **built-in Router**: a first-class
> internal component whose data plane is the **openrusty h2c gateway**
> embedded as an in-process library (`link_repos/openrusty` —
> `openrusty-core` / `openrusty-server`). `networking.k8s.io/v1` Ingress
> is the first-class ingress resource and the source of truth for routing
> (T5.4). The Router remains exposed as a **platform configuration
> variable** (T5.7) readable by node/scheduling/netpol code.
>
> The original M1 slice was the **first vertical** after foundation (Q5),
> built on mlua/Lua. **Q32 deleted that plane**: mlua, the
> coroutine↔async bridge, the phase pipeline (`ngx.*`, `resty.*`,
> cosocket) and `scripts/router-coroutine-selftest.sh` are gone; the
> per-card historical evidence below is retained as record only.
> Externally observable Service behavior (503-on-empty-Endpoints, ST1-ST6)
> is preserved.

Reference: openrusty h2c gateway — `link_repos/openrusty/` (wave 0: lib
targets, `state::from_config`, `reload::apply_config`, `RouteConfig.host`).

---

## T5.1 — openrusty h2c 网关嵌入 (Q32 重写)

- **目标 / Goal**
  Embed the openrusty gateway engine as an **in-process library** behind
  the `router` crate — the h2c/HTTP connection and serving substrate the
  whole layer rides on. (Pre-Q32 this card was "mlua + coroutine↔async
  桥".)

- **核心实现 / Core implementation**
  - `link_repos/openrusty` consumed as path deps: `openrusty-core` +
    `openrusty-server` (wave 0 upstream change: lib targets usable as
    libraries).
  - `crates/router` wraps the engine: `AppState` construction from
    compiled route config (`state::from_config`), served on ONE dedicated
    thread with a current-thread runtime (`router::supervise`);
    every connection task is plain `Send`.
  - mlua / Lua VM / coroutine↔async bridge are **removed** (Q32).

- **验收手段 / Acceptance**
  - Real HTTP/1.1 + h2c connections served by the embedded engine inside
    `init-pro server`; workspace + router integration tests green;
    service-traffic e2e ST1–ST6 on a real cluster.

- **状态 / Status** — done (rewritten per Q32, Sprint 25)
- **证据 / Evidence** — 历史 (pre-Q32, 保留作记录): mlua spike
    (`tests/concurrency.rs` kill-criterion PASS, ADR Q12) +
    `scripts/router-coroutine-selftest.sh`（已删除）。Sprint 25: openrusty
    lib target 嵌入, 上游 203 tests; router crate 现为 reflectors +
    route model + gateways 的薄封装（见 `crates/router/src/lib.rs`
    头注, Q32）。
- **卡点 / Blockers** — none. openrusty becomes a hard dependency (Q32
    consequence); Lua programmability was deliberately scoped out.
- **依赖 / Depends on** — T0.3

---

## T5.2 — HTTP 管线 + phase hooks (openrusty 承载, Q32 重写)

- **目标 / Goal**
  The HTTP request path that routes and proxies traffic. As of Q32 the
  pipeline is openrusty's h2c gateway request handling (connection
  accept → route resolve → upstream forward → relay), not an ordered Lua
  phase chain.

- **核心实现 / Core implementation**
  - openrusty h2c gateway serving both gateway kinds: the Ingress
    gateway and the per-`nodePort` listeners (one `AppState` each).
  - The 503 gate (`crates/router/src/gate.rs`) sits in front of every
    gateway: empty Endpoints → **503** (ST5 parity, preserved from the
    Lua era).
  - Removed with Q32: `init_worker`→`rewrite`→`access`→`content`→
    `header_filter`→`body_filter`→`log` chain, `ngx.*`, request-body
    buffering hooks, cosocket.

- **验收手段 / Acceptance**
  - Integration: a real client reaches the right upstream through the
    embedded gateway; empty-Endpoints Services observe 503
    (`scripts/service-traffic-e2e.sh` ST1–ST6).

- **状态 / Status** — done (rewritten per Q32, Sprint 25)
- **证据 / Evidence** — 历史 (pre-Q32, 保留作记录): Scope A/B phase
    pipeline (ADR Q13/Q14, `tests/phase_chain.rs` 15 +
    `content_phase.rs` 8 + `cosocket_echo.rs` 3)。Sprint 25: 管线由
    openrusty h2c gateway 承载；ST1–ST6 6/6 全绿（503 门保留）。
- **卡点 / Blockers** — none for the swap; per-module wire deviations from
    the Lua era (wildcard hosts, TLS termination, active health probes)
    are documented in the router crate.
- **依赖 / Depends on** — T5.1

---

## T5.3 — 路由模型 + 上游解析 (RouteTable / reflectors / resolver, Q32 重写)

- **目标 / Goal**
  The routing state the gateways serve from: the `RouteTable` route
  model, plus the live Services/Endpoints/Ingress reflectors that feed
  upstream resolution. (Pre-Q32 this card was "resty::* 等价标准库".)

- **核心实现 / Core implementation**
  - `route::RouteTable` — host (exact/wildcard) × path
    (`Prefix`/`Exact`/`ImplementationSpecific`) matching, specificity
    ordering; the compile target of the Ingress compiler (T5.4).
  - `endpoints.rs` + `endpoints_watch.rs` — LIST→WATCH reflectors over
    services+endpoints (gap-free, revision-ordered, re-LIST on stream
    close) folded into `EndpointsResolver`.
  - `ingress_watch.rs` — the `networking.k8s.io/v1` Ingress set
    reflector (Q32: Ingress is the source of truth).
  - Removed with Q32: `router::resty::*`, `ngx.shared.DICT`, cosockets.

- **验收手段 / Acceptance**
  - Endpoint updates re-target live gateways without restart; Service
    delete retires nodePort listeners; ST1–ST6 prove the resolver path
    end to end.

- **状态 / Status** — done (rewritten per Q32, Sprint 25)
- **证据 / Evidence** — 历史 (pre-Q32, 保留作记录): `crates/router/src/resty/`
    标准库子集 + `tests/resty_stdlib/` 24 tests（已删除）。Sprint 25:
    reflectors + `RouteTable` 保留并成为 openrusty `AppState` 的输入；
    ST1–ST6 6/6。
- **卡点 / Blockers** — none. Lua-era programmability (shared dicts,
    cosocket TLS) has no openrusty equivalent in v1 — a deliberate Q32
    scope cut; shared-dict/drain equivalents live under T5.5.
- **依赖 / Depends on** — T5.1, T5.2

---

## T5.4 — 内置 Router 核心 + Ingress (networking.k8s.io/v1) → 路由编译 (Q32 重写)

- **目标 / Goal**
  The Router core: route tables, upstream pools, and a compiler that turns
  Kubernetes `Ingress` into `RouteTable` state the openrusty gateways
  serve. `networking.k8s.io/v1` Ingress is the first-class ingress
  resource and the source of truth (Q32; the Lua fallback pipeline no
  longer exists).

- **核心实现 / Core implementation**
  - `crates/api`: `networking.k8s.io/v1` Ingress + IngressClass models,
    schema-registry + discovery registration (golden `G31` pins
    `/apis/networking.k8s.io/v1` byte-stable).
  - `crates/router`: `ingress.rs` compiles Ingress objects →
    `RouteTable`; `ingress_watch.rs` keeps the Ingress set live;
    `gateway.rs`/`gateway_config.rs` build the openrusty `AppState`
    (`state::from_config`) for the Ingress gateway; endpoint and Ingress
    changes apply through openrusty's in-memory reload
    (`reload::apply_config`, `RouteConfig.host`) — the config-driven
    successor of the Lua-era "hot reload" seam.
  - The 503-on-empty-Endpoints gate wraps both gateway kinds (ST5).
  - NodePort ServiceLB plane (Q28) ported onto the openrusty plane;
    `NodePortConfig.ingress_listen: Option<SocketAddr>` (default `None`)
    exists for the Ingress listener but the **CLI flag is not wired yet**
    (debt, see T5.5/T5.7).

- **验收手段 / Acceptance**
  - Apply an Ingress; `curl` the host → routed to the right Service; a
    second Ingress updates routing with no restart.
  - Pinned in T0.6 golden: `G31` (networking.k8s.io discovery,
    `golden/discovery-networking-v1.json` + regenerated
    `discovery-apis.json`).

- **状态 / Status** — done (rewritten per Q32, Sprint 25: openrusty h2c
    网关嵌入 + Ingress (networking.k8s.io/v1) → 路由编译; remaining:
    live informer wiring beyond the current reflectors + dynamic cert
    handling → T5.5)
- **证据 / Evidence** — 历史 (pre-Q32, 保留作记录): Sprint 8 的
    `route.rs`/`ingress.rs`/`balancer.rs`/`proxy.rs`/`tls.rs`/`conn.rs`/
    `config.rs` + `proxy_routing`/`tls_routing`/`hot_reload` tests
    （其中 balancer/proxy/conn/config/tls 随 Q32 删除，route/ingress
    保留）。Sprint 25: golden 39/39（G31 新增）; service-traffic
    ST1–ST6 6/6 在 openrusty 面上。
- **卡点 / Blockers** — `ingress_listen` CLI flag unwired (debt); TLS
    termination surface is narrower than the Lua era (rustls/SNI swap
    documented per-module in the router crate); full live-informer
    reload path is T5.5.
- **依赖 / Depends on** — T5.2, T5.3, T1.1

---

## T5.5 — 热加载 / 动态配置 (no-restart reload)

- **目标 / Goal**
  Apply route/upstream/TLS changes without dropping the worker or existing
  connections. As of Q32 the seam is openrusty's
  `reload::apply_config` / `state::from_config` (in-memory config apply),
  not the Lua-era generation-swap.

- **核心实现 / Core implementation**
  - Build on what T5.4 already does: reflector state → compiled route
    config → openrusty `apply_config` (endpoint and Ingress changes are
    already live without restart).
  - Remaining scope (openrusty equivalents): a live informer-grade source
    beyond the current LIST→WATCH reflectors; connection drain on
    upstream change (replaces "long-lived cosocket drain"); shared-dict
    equivalent state (openrusty has none in v1); dynamic cert issuance /
    TLS reload equivalent.
  - Wire the `NodePortConfig.ingress_listen` CLI flag (currently
    default-`None` and unwired — T5.4 debt).

- **验收手段 / Acceptance**
  - In-flight request completes with old route while new requests use the
    new route (golden).

- **状态 / Status** — not-started
- **证据 / Evidence** — —
- **卡点 / Blockers** — `ingress_listen` flag unwired (recorded Sprint 25);
    shared-dict/drain/cert equivalents must be re-shaped for openrusty.
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

  - **As-built (Sprint 18, Q28 NodePort scope; Sprint 25 re-platformed
    onto openrusty per Q32):** the kube-proxy-equivalent lives in the
    Router — `crates/router/src/endpoints.rs` + `endpoints_watch.rs`
    run LIST→WATCH reflectors over services+endpoints on the shared
    storage `Arc` feeding an `EndpointsResolver`; `nodeport.rs` starts
    one reverse-proxy listener per allocated nodePort on the openrusty
    h2c plane behind the shared 503 gate (empty Endpoints → 503;
    listener retired on Service delete; Endpoints updates re-target
    without restart). On by default in the server;
    `--disable-kube-proxy` opts out. No ClusterIP dataplane (decision
    **Q28**).

- **验收手段 / Acceptance**
  - Golden: a TCP Service gets a VIP and load-balances across endpoints.
  - As-built (NodePort scope): `crates/router/tests/nodeport_plane.rs`
    (live proxy / 503 / re-target / retire over real TCP backends) +
    `scripts/service-traffic-e2e.sh` ST1–ST6 on a real local cluster.
  - As-built pin (Sprint 29): golden G70-G72 drive the SERVED plane and assert
    the remaining scope executably — the compiled Ingress host-routing table
    has no listener yet (`NodePortConfig::default().ingress_listen: None`, no
    caller sets it); the empty-backend 503 gate and per-service isolation hold.

- **状态 / Status** — in-progress (NodePort plane done, openrusty 承载,
    Sprint 18 Q28 + Sprint 25 Q32)
- **证据 / Evidence** — router endpoints reflector/resolver +
  `nodeport_plane` integration; service-traffic e2e ST1–ST6 6/6
  (re-run green on the openrusty plane, Sprint 25); Q28/Q32 ADRs
- **卡点 / Blockers** — remaining scope: LB VIP allocate/announce +
    TCP/SNI stream routing need the T4.3 dataplane (Scope A flannel is
    in, Q34 — coordinate there); ClusterIP dataplane deferred (Q28).
- **依赖 / Depends on** — T5.4, T4.3

---

## T5.7 — 内置 Router 作为平台配置变量

- **目标 / Goal**
  Expose the Router's state as a **global platform configuration
  variable**: node, scheduler, and netpol code can read Router state,
  reserving a seam for whole-platform policy extensions. (Pre-Q32 the
  seam was a typed Lua context `init-pro.router.var.*`; the openrusty
  equivalent is a typed config/state publication, to be designed.)

- **核心实现 / Core implementation**
  - A typed config/state publication from the Router (openrusty
    equivalent of the pre-Q32 `init-pro.router.var.*` Lua context) to a
    platform config bus (T0.3).
  - Read API consumed by Layer 3/4 components (e.g., node labels derived
    from Router state; scheduling hints; netpol defaults).
  - Versioned schema; changes hot-propagate via T5.5 machinery.

- **验收手段 / Acceptance**
  - Demo: a Router state variable is observable from a scheduler
    extender (T3.2) and influences placement (golden).

- **状态 / Status** — not-started
- **证据 / Evidence** — —
- **卡点 / Blockers** — Scope of "platform policy extension" — keep v1
    read-only from non-Router code. Shares the T5.5 openrusty-reload /
    `ingress_listen` unwiring debt (Sprint 25).
- **依赖 / Depends on** — T5.4, T4.5
