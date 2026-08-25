# Layer 7 — AI Agent & Workflow scheduling (Phase 2)

Mirrors `index.md` TODO IDs **T7.1–T7.3**. Per **Q3**, this layer is gated
behind M5 (Layers 0–6 done). It extends the *generic* cluster with
AI-agent-aware scheduling via the standard scheduler-extender seam (T3.2),
**and** with argo-workflows integrated as a first-class built-in so
`Workflow`/`CronWorkflow` are created and scheduled natively by init-pro
(T7.3).

---

## T7.1 — AI Agent workload CRD + scheduler extender

- **目标 / Goal**
  A `init-pro.io/v1alpha1` `AgentWorkload` CRD and an out-of-process scheduler
  extender (upstream-compatible) that places agent pods by agent-specific
  semantics (affinity to peers, context locality, model availability).

- **核心实现 / Core implementation**
  - CRD with spec for agent group, model/image, context refs, SLO hints.
  - Extender implements the upstream HTTP filter/score protocol; hooks
    into T3.2's extender seam.
  - Controller (T3.1 pattern) materializes `AgentWorkload` → Pod(s).

- **验收手段 / Acceptance**
  - Golden: schedule a co-located agent group; extender scored nodes per
    policy; status reflects agent-specific conditions.

- **状态 / Status** — not-started
- **证据 / Evidence** — —
- **卡点 / Blockers** — CRD API design not finalized; defer until M5.
- **依赖 / Depends on** — T3.2, T1.2

---

## T7.2 — GPU/资源调度与策略

- **目标 / Goal**
  GPU and accelerator-aware scheduling (nvidia/amd device plugins +
    fractional GPU / time-slicing policies) for agent workloads.

- **核心实现 / Core implementation**
  - Device-plugin contract (gRPC) to advertise GPUs as extended resources.
  - Scheduler scoring (T3.2/T7.1) honoring GPU type, memory, time-slice.
  - Optional Router integration (T5.7) to steer ingress by model endpoint.

- **验收手段 / Acceptance**
  - Golden (best-effort, requires hardware): a GPU-flagged agent pod lands
    on the GPU node; time-slicing limit enforced.

- **状态 / Status** — not-started
- **证据 / Evidence** — —
- **卡点 / Blockers** — Hardware-dependent acceptance; provide a mock
    device plugin for CI.
- **依赖 / Depends on** — T7.1, T4.2

---

## T7.3 — argo-workflows 最小化整合 (Workflow/CronWorkflow 一级公民内置)

- **目标 / Goal**
  Minimal, first-class integration of argo-workflows: `Workflow` and
  `CronWorkflow` are **built-in** resources that init-pro creates and
  schedules directly — no separate addon/controller deployment. They are
  first-class citizens alongside Pods/Deployments, callable from the same
  API/CLI surface.

- **核心实现 / Core implementation**
  - **As-built (Sprint 21, Scope A, Q30):** `Workflow`/`CronWorkflow` are
    **typed built-in API objects** in group `argoproj.io/v1alpha1`, ported
    full-field 1:1 from upstream Argo Workflows v4
    `pkg/apis/workflow/v1alpha1` into new `crates/api/src/argoproj/`
    (16 modules, ~313 Go json tags; SSOT = `link_repos/argo-workflows` Go
    json tags; embedded k8s core types reused from the `k8s_openapi` crate;
    JSON-only wire per Q10). Conventions: exact omitempty mapping via
    `Option`/`Vec`/`BTreeMap` + skip-helpers, type aliases for Go string
    enums, custom serde only for `AnyString`/`Amount`/`Item`, steps as
    `Vec<Vec<WorkflowStep>>`, `WorkflowStep.inline` as
    `Option<Box<Template>>`, `Plugin` as transparent
    `serde_json::Value`, v4 `schedules: Vec<String>` with a v3 `schedule`
    alias + string-or-vec deser for v3 compat. Registration: shortNames
    workflows→`wf`, cronworkflows→`cwf`/`cronwf` plumbed through new
    `TypeInfo.short_names` + `SchemaRegistry::register_with_short_names()`
    in `crates/api/src/schema.rs` (old `register()` unchanged; discovery
    projects shortNames only when non-empty; wired via
    `crates/cli/src/discovery.rs`). CRUD/SSA/watch come from the existing
    generic apiserver routes via schema registration — zero bespoke
    handlers. kubectl built-ins (`crates/kubectl`): verb-first dispatch
    with minimal `get` / `apply -f` / `delete` verbs (Output table
    rendering, age column, manifest parse incl. multi-doc YAML),
    generalized `request_json` HTTP client (content-type/body; SSA =
    PATCH `?fieldManager=` with `application/apply-patch+yaml`); new dep
    `serde_yaml_ng = "0.10"` (caret-only, libsql precedent).
  - Remaining (Scope B+): embed the argo workflow controller + cron
    controller into the single `init-pro` binary (Q1 multicall; supervisor
    via T0.3), running server-side like other Layer-3 control loops —
    actually reconciling Workflow specs into pods.
  - Reuse init-pro scheduler (T3.2) and kubelet (T4.2) for pod execution —
    argo steps land as pods; no parallel executor.
  - Minimal CRD subset: steps/dag templates, artifacts (object-store backed),
    retries, cron schedules; defer plugins/UI to a later TODO.
  - RBAC + namespace isolation (T1.3); service-account token per workflow.

- **验收手段 / Acceptance**
  - Golden (T0.6): `kubectl apply -f workflow.yaml` (a 2-step DAG) → steps
    run to completion as pods; status reports node phases; artifacts retrievable.
  - `kubectl apply -f cronworkflow.yaml` → fires on schedule; concurrent-run
    policy honored.
  - No external install step: a fresh `init-pro` cluster lists `Workflow`
    via `kubectl api-resources` out of the box.
  - As-built (Scope A): fidelity = 8 round-trip tests in
    `crates/api/tests/argoproj_fidelity.rs` vs 11 upstream-derived fixtures
    (`crates/api/tests/fixtures/argoproj/`) — upstream YAML applies as-is;
    7 integration tests in `crates/apiserver/tests/argoproj_crud.rs`
    (CRUD/SSA/discovery; watch covered by storage-layer tests); 23 kubectl
    verb tests. Golden: G02 `/apis` now lists argoproj.io; NEW **G27**
    (byte-stable `/apis/argoproj.io/v1alpha1` golden) + **G28**
    (Workflow+CronWorkflow CRUD/SSA lifecycle) in
    `scripts/golden/suites/argoproj.sh` — 33/33 green.
  - As-built (S5 smoke hardening): `metadata.generateName`-only POSTs get
    a server-minted 5-char utilrand suffix (`crates/apiserver/src/collection.rs`,
    `getrandom 0.4`); kubectl `apply -f` POST-creates such manifests and
    `-o name` prints qualified `workflow.argoproj.io/<name>`; upstream
    `examples/hello-world.yaml` round-trips apply→get→delete via the
    built-in kubectl.

- **状态 / Status** — in-progress (Scope A done, Sprint 21)
- **证据 / Evidence** — api fidelity 8 (vs 11 fixtures) / apiserver 8 /
  kubectl 25 tests; golden G27/G28 (33/33); Q30 ADR. Workspace 812
  passed / 0 failed; clippy `-D warnings` zero; fmt clean.
- **卡点 / Blockers** — upstream analysis landed (SSOT = upstream Go json
  tags). Scope A does NOT yet run workflows: the workflow controller
  (reconciling Workflow specs into pods), executor semantics, artifact
  storage, and events are Scope B+; argo CLI surface beyond
  get/apply/delete likewise open.
- **依赖 / Depends on** — T1.2, T3.1
