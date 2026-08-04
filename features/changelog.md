# Feature changelog (init-pro)

Feature-centric deltas, reverse chronological. NOT a duplicate of
`CHANGELOG.md` (which is sprint-centric with auditable test counts); this
ties changes to feature cards. Detailed per-sprint history: `CHANGELOG.md`.

- server-side-apply: CREATED (T1.2c) — SSA field-manager over the REST face. `crates/api/src/apply/` (field extraction by merge-key + merge/conflict/prune algorithm + managedFields round-trip) + `crates/apiserver/src/apply.rs` (PUT/PATCH dispatch on `application/apply-patch+yaml`, 201 create / 200 update / 409 conflict / `force` ownership steal). +15 tests (8 api + 7 apiserver); golden G13/G14 → 14/14. T1.2c deferred → done; T1.2 → done.
- storage-layer: CREATED. T2.1/T2.2 - `StorageBackend` trait +
  `EmbeddedStorage` (etcd-faithful revisions + CAS + broadcast watch) + 15
  integration tests. SSOT status table not yet updated to "done".
- router-data-plane: reached the M1 data plane (T5.4) - Ingress->route
  compiler, round-robin balancer, Rust reverse proxy, TLS termination /
  SNI, hot-reload seam. Completes Phase 1 (M0 + M1). `resty::*` + shared
  dicts (T5.3) and the phase pipeline + cosocket (T5.1/T5.2) preceded it.
- discovery-api: byte-correct over HTTP (T1.2a) on top of the T1.1
  resource model; golden conformance gate (T0.6) is 6/6 green.
- build-bundling: landed (T0.2) - pinned-artifact acquire + SHA-256 gate +
  zstd embed codegen + SPDX-2.3 SBOM + runtime `stage()`.
- multicall-cli: done (T0.1 + T0.4) - argv[0] dispatch + reexec peer
  stubs + k3s flag parity (wired / no-op / conflict rules).
