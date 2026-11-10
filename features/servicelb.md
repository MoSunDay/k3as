# servicelb (T4.3 Scope B / T5.6, Q41)

k3s-parity `type=LoadBalancer` with zero external provider - a
klipper-lb equivalent: announce a VIP into `status.loadBalancer.ingress`
and forward `vip:servicePort` to ready Endpoints, both inside the
single binary. Decision: Q41; cards: T4.3 Scope B + the T5.6 LB face;
detail in `plans/init-pro/plan/04-node.md` and
`plans/init-pro/plan/05-ingress-lua.md`.

## Shape

- `crates/controllers/src/controllers/service_lb.rs` (353 lines, the
  announce plane): VIP = explicit `spec.loadBalancerIP` else the first
  Node InternalIP. A Service that already carries a NON-EMPTY
  `status.loadBalancer.ingress` is never rewritten - an external
  MetalLB/cloud controller stays possible. Fresh Service+Node read,
  then CAS status write; a lost CAS requeues rate-limited. A
  single-worker `service_lbs` queue is fed by BOTH Service and Node
  events; `--disable servicelb` removes the reconciler
  (`ManagerConfig::disable_servicelb`).
- `crates/router/src/servicelb.rs` (432 lines, the raw-TCP L4 plane):
  one listener per `vip:servicePort` round-robin over ready Endpoints
  on a dedicated host worker thread, re-driven on Services/Endpoints
  folds. Bind failure = announce-only with ONE warn per key (the VIP
  usually belongs to the provider). An accepted connection with an
  empty backend gets an immediate FIN. Drain aborts the accept loops;
  per-connection byte-pipes die with the runtime.
- `crates/router/src/nodeport.rs`: the NodePort plane admits
  LoadBalancer-type Services too (an LB builds on NodePort in k8s);
  `endpoints.rs::parse_service` exposes the Service type + the
  announced ingress IP (empty-string filtered).

## Evidence

- +13 tests at landing: 9 announce reconciler (VIP selection,
  never-rewrite escape hatch, CAS retry), 3 L4 plane (round-robin,
  empty-backend FIN, fold re-drive), 1 endpoints fold.
- Landing tree 984 passed / 0 failed; current tree 991 / 0. Clippy
  `-D warnings` zero. `scripts/golden-conformance.sh` 73 passed /
  0 failed, exit 0.

## Limits / next steps

- TCP only at the L4 face; UDP LoadBalancer ports are a follow-up
  (NodePort/ServiceLB parity pass).
- The VIP is a node address by default (no ARP/BGP announcement) -
  `spec.loadBalancerIP` is the intranet escape hatch for a real VIP.
- No sessionAffinity / externalTrafficPolicy on the LB face (same
  deferral as the NodePort plane, Q28 decision D).
