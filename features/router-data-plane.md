# router-data-plane

The flagship feature: a built-in Router whose data plane is the
openrusty h2c gateway embedded in-process (Q32; it was the
openresty-style mlua/Lua plane until Sprint 25). It is releasable
independently as an ingress gateway. Lives in `crates/router/`.
`#![forbid(unsafe_code)]` throughout.

## Components (by task)

- T5.1 (done) - openrusty embedded as a library (`openrusty-core` /
  `openrusty-server`, wave 0 lib targets + `state::from_config`);
  served on one dedicated thread with a current-thread runtime. The
  pre-Q32 mlua VM + coroutine<->async bridge (Q12) is deleted.
- T5.2 (done) - the HTTP path is openrusty's gateway request handling
  behind the shared 503-on-empty-Endpoints gate (ST5 parity). The
  pre-Q32 Lua phase pipeline (`init_worker`->...->`log`, `ngx.*`,
  Q13/Q14) is deleted.
- T5.3 (done) - route state: `RouteTable` + services/endpoints and
  Ingress reflectors folded into `EndpointsResolver`. The pre-Q32
  `resty::*`/`ngx.shared.DICT`/cosocket stack (Q15) is deleted.
- T5.4 (done) - Ingress (networking.k8s.io/v1, first-class per Q32)
  -> `RouteTable` compiler, nodeport + Ingress gateways as openrusty
  `AppState`s, live endpoint/Ingress changes applied via openrusty's
  in-memory reload (`reload::apply_config` / `RouteConfig.host`).
- T5.6 (in progress) - NodePort service plane (Sprint 18, Q28; now on
  the openrusty plane): the kube-proxy-equivalent lives here — see the
  dedicated section below. LB VIP scope remains.

## Module map (crates/router/src/)

`ingress.rs` (`compile_ingress`) | `route.rs` (`RouteTable`, matchers) |
`endpoints.rs` / `endpoints_watch.rs` (Services+Endpoints reflectors,
Sprint 18) | `ingress_watch.rs` (Ingress reflector, Q32) |
`gateway.rs` / `gateway_config.rs` (Ingress gateway `AppState`) |
`nodeport.rs` (per-nodePort listeners, Q28) | `gate.rs` (503 gate) |
`serve.rs` (listener + serve loop). Deleted with Q32: `vm.rs`,
`pipeline/`, `ngx/`, `resty/`, `cosocket.rs`, `balancer.rs`,
`proxy.rs`, `tls.rs`, `conn.rs`, `context.rs`, `config.rs`.

## NodePort service plane (Sprint 18, Q28)

Service traffic is carried by the Router, NodePort-only — there is NO
ClusterIP dataplane (`--service-cidr` stays a noop; ClusterIP Services
are creatable/storable but non-forwarding).

- `endpoints.rs` + `endpoints_watch.rs` — LIST→WATCH reflectors over
  services+endpoints on the SAME storage `Arc` as the apiserver
  (gap-free, revision-ordered, informer-style re-LIST on stream
  close), folded into `EndpointsResolver` (numeric/named/identity
  targetPort).
- `nodeport.rs` — one reverse-proxy listener per allocated nodePort of
  every NodePort/LoadBalancer Service, served on the openrusty h2c
  plane (Sprint 25, Q32) behind the shared 503 gate; Endpoints
  updates re-target without restart, Service delete retires the
  listener, empty Endpoints → **503**.
- On by default in the server; `--disable-kube-proxy` (k3s-parity
  flag) turns it off. Wired in `crates/cli/src/runtime.rs`.
- Proof: `tests/nodeport_plane.rs` (live proxy / 503 / re-target /
  retire over real TCP backends) + `scripts/service-traffic-e2e.sh`
  ST1-ST6 on a real local cluster.

## Acceptance

Integration tests in `crates/router/tests/`: `nodeport_plane`
(Sprint 18, Q28) and `ingress_gateway` (Q32 Ingress gateway, incl.
the networking.k8s.io discovery path behind golden G31); the
service-plane wire behavior gated by
`scripts/service-traffic-e2e.sh` ST1-ST6 (preserved across the Q32
re-platform).

## Status / next

The openrusty plane is live (Q32). Deferred to T5.5 / Phase 2: a live
informer-grade config source beyond the current LIST→WATCH reflectors,
connection drain, a shared-state equivalent, dynamic cert handling, and
wiring the `ingress_listen` CLI flag (`NodePortConfig.ingress_listen`
exists, default `None`).
