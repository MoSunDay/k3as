# registry (T6.3, Q40)

Embedded Docker Registry HTTP API v2 image plane: `docker`/`containerd`/
`crictl` push and pull from the `init-pro server` process itself, no
external registry install. On top of the protocol: per-repository tag
retention — keep the newest 10 versions by default, with keep-forever
patterns excluded from the count. Decision: Q40; card: T6.3; detail in
`plans/init-pro/plan/06-addons.md`.

## Shape

- `crates/registry`: `validate.rs` (reference grammar), `store.rs`
  (global blob CAS + per-repo manifests + tag records), `policy.rs`
  (pure glob + `prune_plan`), `gc.rs` (retention + blob sweep),
  `api/` (`mod.rs` V2 router + plumbing, `manifest.rs`/`blob.rs`
  handlers), `uploads.rs` (sessions/mount).
- Wiring mirrors anyhome (Q36): default-on in `init-pro server`,
  `--registry-addr` (default `127.0.0.1:15000`), `--disable-registry`;
  spawned after anyhome, drained before the API surface on shutdown.
- On-disk layout `<data-dir>/registry/`: `blobs/sha256-<hex>` (global
  CAS; cross-repo mount is metadata-free), `repos/<name>/manifests/`,
  `repos/<name>/tags/<tag>.json`, `uploads/<uuid>`,
  `keep.json`.

## Retention (the tag-management contract)

- Every manifest push enforces per-repo retention: countable tags =
  all tags minus keep-forever matches; keep the newest `retain`
  (default 10) by `(pushed_at, seq)`; drop the rest.
- Keep-forever patterns: `--registry-keep-tag` (repeatable
  `repo:tag`, `*` wildcard) unioned with `<data-dir>/registry/keep.json`
  on every GC pass (hot reload, no restart). A bare `repo` pins the
  whole repository. Pinned tags never consume the retention budget.
- `--registry-tag-retention 0` = unlimited.
- The `*` glob matcher backtracks (standard two-pointer star match over
  `char`s), so `x*aa*a` matches `xaaa` — a pinned tag is never
  mis-pruned by a false negative (and its blobs never GC'd as a result).
- After tag pruning: manifests that lost their last tag die, then
  blobs referenced by no stored manifest are reclaimed with a 600 s
  freshness grace (protects a concurrent upload->push pair).

## Wire surface

- `GET /v2/` (ping + `Docker-Distribution-API-Version`),
  `GET /v2/_catalog`, `GET /v2/<name>/tags/list`.
- Manifests `GET/HEAD/PUT/DELETE` by tag or digest (PUT: docker-v2 /
  OCI manifest/list/index; config/layer/index-child digests must pass
  the `sha256:<64 lowercase hex>` grammar gate first — 400
  `MANIFEST_INVALID` before any filesystem probe — then exist ->
  `MANIFEST_BLOB_UNKNOWN`). Delete-by-tag is refused
  (`DIGEST_INVALID`); delete-by-digest drops the tags too, and a digest
  that matches no stored manifest and no tag answers 404
  `MANIFEST_UNKNOWN` (202 only when something was removed).
- Blobs `GET/HEAD/DELETE`; `GET` streams the CAS file from disk with an
  explicit `Content-Length` + digest (a large layer is never buffered
  whole; Range/Accept-Ranges is a follow-up). Uploads `POST` (monolithic
  `?digest=` or mount `?mount=&from=`), `PATCH` chunk appends with
  `Content-Range` offsets, `PUT ?digest=` finalize (stream-hashed),
  `DELETE` cancel. Manifest/blob GET/HEAD/PUT responses carry
  `Docker-Content-Digest`.
- Shutdown drain is bounded at 10 s; on timeout the registry listener
  is aborted with a warn, so a stalled upload cannot wedge SIGTERM.

## Evidence

- `crates/registry` 18 tests (12 unit + 6 HTTP integration over a real
  socket across `tests/v2_http.rs` and `tests/v2_manifest.rs`:
  push/pull, 12->10 retention, keep.json pin, delete flows, a 2 MiB
  streamed blob GET, malformed-digest 400s).
- `scripts/registry-e2e.sh`: default 7 passed / 0 failed / 1 skipped
  (R7 real `crictl pull` env-gated via `INIT_PRO_E2E_CONTAINERD=1`);
  R7 was proven with a live containerd in Sprint 34 -> 8 passed /
  0 failed / 0 skipped (pulled image `a19229a77077b`, 718 B).
- `scripts/cli-flag-parity-test.sh`: 20/0 incl. the `t-registry`
  live-help gate.

## Limits / next steps

- No auth: loopback-by-default listener, intranet h2c posture (Q38);
  multi-node clusters pass `--registry-addr 0.0.0.0:<port>` (v1 assumes
  bind == advertise).
- Agent-side containerd mirror config for the registry is a follow-up;
  node-local pulls work against the default address already.
- Blob GC scans all stored manifests per push — fine at v1 scale; a
  background cadence is the obvious next step if repositories grow.
- `_catalog` is unpaginated at v1.
- Multi-arch index children are pushed untagged, so they never age out
  (permanent retention) — a follow-up.
- `DELETE blob` removes the global CAS file repo-wide (see above).
- Range/Accept-Ranges on blob GET is a follow-up (full-body streaming
  with explicit `Content-Length` today).
