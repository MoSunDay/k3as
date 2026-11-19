# Layer 8 — Delivery plane (CI/CD + traffic shaping)

Mirrors `index.md` TODO IDs **T8.1–T8.5**. Layer 8 is the *delivery* plane: how a
service's code becomes a running image and how traffic is shifted onto the new
revision. It builds on the embedded registry (T6.3/Q40), the Router data plane
(T5.x/Q32) and the controller framework (T3.1).

Milestones:
- **M1** = T8.1 (image delivery trust + push plane) + T8.2 (TrafficSplit) plus
  their API CRDs — done (Sprint 45).
- **M2** = T8.3 core (ServicePipeline/Build controller + executor) + T8.4 wiring
  (CLI flags, default-on posture, in-process e2e) — done (Sprint 45).
- **M3** = T8.5 canary release (DRAFT **Q58**, controller not started).

Decisions: **Q56** (ServicePipeline/Build), **Q57** (TrafficSplit), **Q58**
(canary, DRAFT). The plain-HTTP image posture is inherited from **Q38**
(intranet h2c) + **Q40** (embedded registry), not a separate decision.

---

## T8.1 — 镜像交付信任 + registry push 面 (M1)

- **目标 / Goal**
  Make the embedded registry (T6.3/Q40) usable end to end by both runtimes:
  containerd learns the *pull* face through a rendered `certs.d/<host>/hosts.toml`
  (plain HTTP, no TLS/auth in v1), and the build plane learns the *push* face
  through a dependency-free Docker Registry V2 push client plus a `docker save`
  tarball importer. Groundwork for T8.3; no new API surface.

- **核心实现 / Core implementation**
  - containerd registry face: `crates/runtime/src/config.rs` —
    `registry_host_key()` (scheme/slash-free host key, containerd `certs.d`
    form) + `render_hosts_toml()` (`server = "http://<host>"`, `[host."…"]`,
    pull+resolve+push capabilities) and the `registry` stanza templating
    (`ContainerdConfigVars::registry_config_path` → `config_path = <certs.d>`);
    `crates/runtime/src/lib.rs` `start_agent_runtime_with()` renders
    `<data-dir>/agent/containerd/certs.d/<host>/hosts.toml` and points the CRI
    plugin at the `certs.d` root — no `/etc/docker/daemon.json` edits.
  - push client: `crates/registry/src/client.rs` (canonical blob dance
    `POST /v2/<name>/blobs/uploads/` → `PATCH` → `PUT ?digest=` → `PUT
    /v2/<name>/manifests/<tag>`, config+layer media types, `https://`
    rejected outright), `crates/registry/src/http.rs` (one `Connection: close`
    HTTP/1.1 exchange per request, Content-Length framing, relative `Location`
    resolution), `crates/registry/src/tarimg.rs` (`docker save` archive
    importer: `manifest.json` → config + layer members, gzip/zstd detected by
    magic bytes on the flate2/zstd stack of **Q54**, 8 GiB bomb guards).
  - kubectl/discovery untouched: T8.1 adds no Kind, so the golden discovery
    fixtures and the kubectl verb table are unchanged.

- **验收 / Acceptance**
  - `crates/registry/tests/push_client.rs` — real in-process registry + synthetic
    `docker save` tarball + pure-Rust `push_image` end to end over plain HTTP
    (digest shape, tag visibility, byte-exact stored manifest, second push is
    the "201 both times" assertion).
  - `crates/runtime/src/config.rs` unit tests
    (`hosts_toml_is_plain_http_and_push_capable`,
    `hosts_toml_keeps_an_explicit_scheme`) + containerd config render tests.

- **状态 / Status** — done (Sprint 45, M1).
- **证据 / Evidence** — `cargo test -p registry -p runtime --locked`.
- **卡点 / Blockers** — none. TLS/auth for the push face is deliberately out
  (intranet h2c posture, Q38); a real `X-Registry-Auth`/bearer flow would be a
  separate item.
- **依赖 / Depends on** — T6.3 (registry server), T4.1 (containerd bundling).

---

## T8.2 — TrafficSplit: 加权一键切流 (M1, Q57)

- **目标 / Goal**
  A first-class `init-pro.io/v1` `TrafficSplit` (short name `ts`): `spec.service`
  names a NodePort entry Service (the listener owner), `spec.backends[]` name
  backend Services with relative weights. The built-in Router folds splits into
  its resolver state and rewrites the openrusty upstream weights with a hot
  reload — no service mesh, no external CRD.

- **核心实现 / Core implementation**
  - API: `crates/api/src/traffic.rs` — `TrafficSplit`/`TrafficSplitSpec`/
    `TrafficSplitBackend`/`TrafficSplitStatus`, schema registration (plural
    `trafficsplits`, shortName `ts`, namespaced), JSON-only (Q10).
  - Router planner: `crates/router/src/split.rs` — `SplitView::from_api`,
    `find_split` (ns-qualified first, then bare-name match), `plan_for`
    (drop `weight <= 0` backends, resolve each backend's ready peers, divide the
    weight across endpoints scaled by 100 with a `basis % n` remainder to the
    first endpoints and a floor of 1, sorted/deduped peer list so identical
    edits do not reload), `endpoint_weights`, `uniform` (no-split fallback).
  - Reflector: `crates/router/src/traffic_watch.rs` — LIST → WATCH → re-LIST on
    close, folding `init-pro.io/trafficsplits` into `ResolverState::splits`;
    idempotent folds make replayed events harmless.
  - NodePort fold: `crates/router/src/nodeport.rs` re-plans peers per listener on
    each fold (gate `gate.rs`: a split listener stays healthy while any backend
    still has peers; `gateway_config.rs` emits the weighted upstream).
  - kubectl: short-name/verb resolution (`get`/`apply`/`delete`) for
    `trafficsplit|trafficsplits|ts`.

- **验收 / Acceptance**
  - `crates/router/tests/nodeport_plane.rs` — a TrafficSplit on the entry
    Service switches traffic by weight through the live NodePort plane.
  - `crates/router/src/split.rs` unit tests (weight scaling, remainder,
    zero-weight drop, deterministic order) + `traffic_watch` fold tests.
  - kubectl short-name tests (`crates/kubectl/src/verbs_tests.rs`) and wire tests
    (`verbs_wire_tests.rs`): `/apis/init-pro.io/v1/namespaces/<ns>/trafficsplits`.
  - golden discovery: `golden/discovery-initpro-v1.json` carries
    `trafficsplits` (`ts`); `scripts/apiserver-discovery-parity-test.sh` asserts
    it.

- **状态 / Status** — done (Sprint 45, M1, Q57).
- **证据 / Evidence** — `cargo test -p router -p kubectl --locked`;
  `scripts/golden-conformance.sh`.
- **卡点 / Blockers** — header/cookie pinning needs openrusty header match
  (future item); zero-weight backends are dropped, not drained gracefully.
- **依赖 / Depends on** — T5.4 (Router + hot reload, Q32), T1.1 (API schema).

---

## T8.3 — ServicePipeline/Build 内嵌 CI/CD 控制器 + 执行器 (M2 core, Q56)

- **目标 / Goal**
  One `ServicePipeline` per service: image repository+tag, source (docker build
  context or prebuilt `docker save` archive), optional target
  Deployment/container, build knobs. A tag transition creates exactly ONE
  immutable `Build` child; the child runs build → push → (best-effort) Deployment
  image patch. Embedded plane: no external CI daemon.

- **核心实现 / Core implementation**
  - API: `crates/api/src/pipeline.rs` (`ServicePipeline`, `sp`;
    `PipelineImage`/`PipelineSource`/`PipelineTarget`/`PipelineRelease`,
    phase constants, `RolloutStatus` types for M3) and
    `crates/api/src/build.rs` (`Build`, `bld`: immutable spec, `status.phase`/
    `digest`/`logTail`).
  - Controller: `crates/controllers/src/controllers/pipeline/` —
    `mod.rs` (pipeline reconcile: `spec.image.tag` vs `status.observedTag`, one
    child named `<pipeline>-<tag>-<random>`, `Failed` for the same tag held,
    promotion patches the target Deployment and sets `Promoted`),
    `build.rs` (phase machine `Pending`/`Building` → `Publishing` →
    `Succeeded`|`Failed`; context mode = `docker build` + `docker save`,
    archive mode = docker-free; push via `registry::push_image`),
    `plan.rs` (+ `plan/tests.rs`: pure naming/owner/phase/image-patch/timeout
    decisions), `status.rs` (full-object CAS, no `/status` subresource, Q10;
    `logTail` bounded to 4096 chars), `executor.rs` (`tokio::process::Command`
    seam, `tokio::time::timeout` + `kill_on_drop`; failures are values, not
    `Err`), `publish.rs` (`registry_and_repo`: host-full vs host-less
    repository resolution against `--registry-addr`), `tests.rs` (unit + fake
    client integration).
  - Runner wiring: `ManagerConfig.pipeline` (`Some`/`None`),
    `Caches.servicepipelines`, queues `servicepipelines` + `builds` (one worker
    each), 2 s pipeline resync (promotion waits on a Deployment rollout), build
    events enqueue their owner ServicePipeline, build children get no resync so
    a docker build is never double-started.
  - Housekeeping: `gc.rs` (Build is a dependent kind of ServicePipeline;
    cascade-delete on owner removal) and `namespace.rs` `DRAIN_KINDS`
    (`init-pro.io/builds`, `init-pro.io/servicepipelines`).
  - kubectl verbs + golden discovery for `servicepipelines`/`builds` (short
    names `sp`/`bld`).

- **验收 / Acceptance**
  - `cargo test -p controllers -p api --locked`: pipeline unit tests
    (naming, held-failure, phase machine, `registry_and_repo`, bounded log,
    status CAS) + fake-client integration tests.
  - kubectl verb/wire tests for `sp`/`bld`; golden discovery carries
    `servicepipelines` + `builds`.
  - `scripts/apiserver-discovery-parity-test.sh` green.

- **状态 / Status** — done (Sprint 45, M2 core, Q56).
- **证据 / Evidence** — the test targets above; T8.4 adds the in-process e2e.
- **卡点 / Blockers** — context mode shells out to the host `docker` CLI (single
  node posture; multi-node build distribution explicitly out of scope); no
  multi-arch or build-cache knobs in v1; a failed tag is held, so a retry means
  a new tag.
- **依赖 / Depends on** — T8.1 (push client + `docker save` importer), T6.3
  (embedded registry), T1.2 (apiserver CAS), T3.1 (informer/workqueue
  framework).

---

## T8.4 — pipeline 面收尾: CLI flags + 进程内 e2e (M2, Q56)

- **目标 / Goal**
  Make the pipeline plane deployable and default-on like the other embedded
  planes (anyhome, registry), and prove the whole chain in-process without a
  docker daemon.

- **核心实现 / Core implementation**
  - CLI: `--disable-pipeline` (opt-out; the plane is default-on) and
    `--pipeline-dir` (env `INIT_PRO_PIPELINE_DIR`, default `<data-dir>/pipeline`)
    on `server`; the docker binary comes from `INIT_PRO_DOCKER`, the default
    build/save timeout is 900 s (`spec.timeoutSeconds` overrides per Build).
  - Wiring: `crates/cli/src/lib.rs` builds
    `controllers::PipelineConfig` only when the plane is enabled; `None` = no
    pipeline workers, no informers, no enqueues (pre-T8.3 behavior).
  - Flag parity: the live `--help` gate pins the two flags
    (`scripts/cli-flag-parity-test.sh`, line `t83`) so they survive the
    no-op-strip filter and never regress into "unknown flag".

- **验收 / Acceptance**
  - `crates/controllers/tests/pipeline.rs` — in-process e2e: a real registry v2
    server + a fake `docker` shim (archive mode) drive a ServicePipeline to a
    `Succeeded` Build, the tag is pushed into the registry, the target
    Deployment's container image is patched, and the pipeline reports
    `Promoted`/`observedTag`.
  - `scripts/cli-flag-parity-test.sh` (`t83` live `--help` line) green.

- **状态 / Status** — done (Sprint 45, M2, Q56).
- **证据 / Evidence** — `cargo test -p controllers --locked` (the e2e target) +
  `scripts/cli-flag-parity-test.sh`.
- **卡点 / Blockers** — the e2e deliberately uses archive mode (no docker
  daemon in CI); context mode is covered by the executor unit seams only.
- **依赖 / Depends on** — T8.3.

---

## T8.5 — canary 发布控制器 (M3, Q58 DRAFT)

- **目标 / Goal**
  Pipeline-owned canary rollout: `ServicePipeline.spec.release` drives a
  `<deployment>-canary` Deployment plus a canary Service cloned from the stable
  one, steps traffic weights through a `TrafficSplit` (T8.2/Q57) with soak
  pauses, then promotes or aborts.

- **核心实现 / Core implementation**
  - API types already exist since M1 (`crates/api/src/pipeline.rs`):
    `PipelineRelease` (`trafficSplit`, `stableService`, `canaryService`,
    `canaryDeployment`, ordered `steps[]`), `ReleaseStep`
    (`{weight, pauseSeconds}`), `RolloutStatus` (`step`/`weight`/`since`) and
    the `Rolling`/`Promoted`/`Aborted` phase constants.
  - Planned controller (not started): create/clone the canary Deployment +
    Service, apply each step's weight to the TrafficSplit, wait
    `pauseSeconds`, advance `status.rollout`; **promote** = patch the stable
    Deployment image to the new tag and set the canary weight to 0;
    **abort** = delete the canary objects and set the weight to 0.
  - Explicitly: the fields are parsed and exposed by discovery today, but NO
    reconciler consumes `spec.release` — the controller is M3.

- **验收 / Acceptance** (planned)
  - In-process e2e mirroring T8.4: a release with 2-3 steps reaches `Promoted`
    with the stable Deployment on the new image and the TrafficSplit weight back
    at 0/100; an aborted release removes the canary objects and never touches
    the stable image.

- **状态 / Status** — not-started (M3; DRAFT Q58).
- **证据 / Evidence** — API types + schema/discovery coverage only.
- **卡点 / Blockers** — Q58 is not locked: canary naming/ownership, pause clock
  source (wall clock, no cron), auto-abort policy and the canary replica knob
  are open; depends on all of T8.2/T8.3/T8.4 landing first.
- **依赖 / Depends on** — T8.2, T8.3, T8.4.

---

## 依赖图 / Layer DAG

```text
deps                    -> TODO
T6.3, T4.1              -> T8.1  (M1)
T5.4, T1.1              -> T8.2  (M1)
T8.1, T6.3, T1.2, T3.1  -> T8.3  (M2 core)
T8.3                    -> T8.4  (M2)
T8.2, T8.3, T8.4        -> T8.5  (M3, DRAFT Q58)

flow:  T6.3 ─> T8.1 ─> T8.3 ─> T8.4 ─┐
       T5.4 ─> T8.2 ─────────────────┴─> T8.5
```
