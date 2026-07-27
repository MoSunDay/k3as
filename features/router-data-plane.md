# router-data-plane

The flagship feature: a built-in openresty-style Lua data plane. This is
the M1 vertical slice de-risked first (Q5) and the most mature subsystem;
it is releasable independently as an ingress gateway. Lives in
`crates/router/`. `#![forbid(unsafe_code)]` throughout.

## Components (by task)

- T5.1 (done) - mlua VM + coroutine<->async bridge. A Lua coroutine yields
  at a Rust `await` point on a Tokio `LocalSet`, letting other coroutines
  run concurrently on the same worker VM (openresty's model, in Rust). ADR
  Q12.
- T5.2 (done) - full phase pipeline: `init_worker` + `rewrite` -> `access`
  -> `content` -> `header_filter` -> `body_filter` -> `log`, with
  `ngx.req`/`ngx.header`/`ngx.status`/`ngx.var`/`ngx.exec`/`ngx.redirect`/
  `ngx.arg` and request-body reading, over a real-TCP HTTP/1.1 data plane.
  ADRs Q13/Q14.
- T5.3 (done) - `resty::*` stdlib subset + `ngx.shared.DICT` (auto-created
  shared dicts). ADR Q15. Includes the cosocket (`ngx.socket.tcp`).
- T5.4 (done) - Ingress->route compiler, round-robin `Balancer` +
  `UpstreamResolver`, Rust reverse proxy (`serve_proxy`), TLS termination /
  SNI (rustls `ring` provider, ADR Q16), and a hot-reload seam
  (`ConfigSource` / `reload_channel` / `RouteStore`).

## Module map (crates/router/src/)

`vm.rs` (`worker_vm`) | `pipeline/` (`Phase`, `PipelineBuilder`) | `ngx/` |
`resty/` | `cosocket.rs` | `ingress.rs` (`compile_ingress`) |
`route.rs` (`RouteTable`, matchers) | `balancer.rs` (`Balancer`, `pick_peer`)
| `proxy.rs` (`serve_proxy`, `ProxyOptions`) | `tls.rs`
(`SniCertResolver`, `build_server_config`) | `config.rs` (reload seam) |
`serve.rs` (`serve`, `ephemeral_listener`) | `context.rs` (`RequestContext`).

## Acceptance

Integration tests in `crates/router/tests/`: `proxy_routing`, `tls_routing`,
`hot_reload` (the M1 spike verdict, Q5), plus `concurrency`, `phase_chain`,
`content_phase`, `cosocket_echo`, `sleep_latency`, and `resty_stdlib`.

## Status / next

M1 complete (Phase 1). Deferred to T5.5 / Phase 2: a live `kube-rs`
informer to feed Ingress->routes from the real API server, and dynamic
mid-handshake Lua cert issuance (`ssl_certificate_by_lua`). The
`SniCertResolver` seam is already in place for the latter.
