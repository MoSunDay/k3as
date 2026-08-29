# Layer 7 — AI Agent & Workflow scheduling (Phase 2)

Mirrors `index.md` TODO IDs **T7.1–T7.3**. Per **Q3**, this layer is gated
behind M5 (Layers 0–6 done). It extends the *generic* cluster with
AI-agent-aware scheduling via the standard scheduler-extender seam (T3.2),
**and** with argo-workflows integrated as a first-class built-in so
`Workflow`/`CronWorkflow` are created and scheduled natively by init-pro
(T7.3).

---

## T7.1 — AI Agent workload (init-pro.io/v1 `Agent` 一级资源; Q35 重写)

- **目标 / Goal**
  Phase A: `init-pro.io/v1` `Agent` 一级资源 —— spec 携带与 Pod 相同的调度对齐
  字段（nodeName/nodeSelector/tolerations/affinity/schedulerName/resources,
  camelCase）+ opencoder 载荷（agent/prompt/model/provider, 无凭证），由内置
  调度器以与 Pod 相同的 filter/score/CAS 循环绑定节点；Workflow `template.agent`
  直接创建 Agent 子对象（不再物化 Pod）。opencoder 以库形式收编（`crates/agent`
  纯 seam）。Phase B（节点侧 watch 已绑定 Agent → `opencoder_session::run_once`
  → status CAS 回写；CLI run 模式）留 B1/B2。

- **核心实现 / Core implementation**
  - `crates/api/src/agent.rs`: Agent/AgentSpec/AgentStatus/AgentCondition/AgentList,
    `register_with_short_names(..., "agents", "AgentList", Namespaced, ["ag"])`。
  - scheduler: agents informer + 专用 agent 队列/worker；`is_agent_pending`；
    `Snapshot::build_with_agents`（agents 信息性, Phase A 不占节点容量, Pod 求和
    逐字节不变）；`bind_object`/`mark_unschedulable_object` 按 group/resource/
    condition 参数化 → `bind_agent`/`mark_agent_unschedulable`（Scheduled 条件,
    CAS + write-if-changed 语义保持）；node-Put 扇出 + 30s backstop 覆盖 agents。
  - controllers: `StepTarget::{Pod,Agent}`（agent 优先于 container）；`workflow_agent.rs`
    构建 Agent 子对象（Workflow/argoproj.io/v1alpha1 ownerReference）；阶段聚合
    兼读 `init-pro.io/agents` + `status.phase`→node phase 映射；GC `DEPENDENT_KINDS`
    增 `("init-pro.io","agents")`（级联删除）；runner.rs 765→220 行，拆出
    `runner_wiring.rs`(377)/`runner_worker.rs`(261)。
  - kubectl: `resolve_target(kind)` → `("init-pro.io/v1","agents")`；
    get/apply/delete 通路参数化（argo 路径逐字节不变）。
  - opencoder: `link_repos/opencoder` path-dep（core/llm/store/session；store 牵
    libsql 捆绑 C 构建——接受，同 rdb 先例）；`crates/agent` 纯 seam
    （AgentSpec→`opencoder_core::Config`, 无 I/O/env, 凭证不进 spec）。

- **验收手段 / Acceptance**
  - api/cli/apiserver/scheduler/controllers/kubectl 新增单测+集成测试全绿
    （api: register/round-trip/wire-shape/apply-decode；apiserver: agents CRUD
    + watch 集成（零 src 改动）；scheduler: Agent 选节点 + 无节点→Scheduled=
    False/Unschedulable→节点出现后绑定；controllers: agent 模板→Agent 子对象
    +ownerRef、GC 级联；kubectl: kind 解析 + wire 路径）。
  - golden G04 fixture 再生成（仅增 agents 项, LuaRouter 字节不变），
    `scripts/golden-conformance.sh` 39/39；kubectl apply/get/delete agents 实测通过。

- **状态 / Status** — done (Phase A, Sprint 26, Q35)；Phase B（B1 节点执行 / B2 CLI run）not-started
- **证据 / Evidence** — `cargo test --workspace --locked` 882 passed / 0 failed
  （基线 852 + 30 新增）；`scripts/golden-conformance.sh` 39 passed / 0 failed
- **卡点 / Blockers** — Phase B 状态回写走 merge-patch（status 子资源合并语义
  为 Q34 已记同款债务）；argv[0] `agent` 已被节点代理占用（B2 需新子命令/别名）
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
  - **As-built (Sprint 22, Scope B, Q30):** the **Workflow controller**
    runs in-process inside `ControllerManager` (Q19 pattern:
    informer/workqueue over `StorageClient`; no HTTP client) — new
    `crates/controllers/src/controllers/` modules `workflow.rs`
    (308 lines, reconcile loop), `workflow_plan.rs` (290,
    steps/Pod-entrypoint plan compilation), `workflow_dag.rs` (157,
    Kahn topological layering from `dependencies`/simple `depends` —
    `&&`-split names, `.Succeeded`-style suffixes tolerated),
    `workflow_pod.rs` (206, pod JSON assembly: template-over-workflow
    overlay, restartPolicy Never, labels
    `workflows.argoproj.io/workflow` + `/template`, annotation
    `workflows.argoproj.io/node-name`, controller ownerReference),
    `workflow_status.rs` (171, node-tree builders), `workflow_phase.rs`
    (112, pure naming/phase math). Wiring: `runner.rs` (594 lines —
    Kind::Workflow queue + informer + pod-event `enqueue_owner`
    fan-out + 5s resync), `controllers/mod.rs` (`Caches.workflows`),
    `gc.rs` (Workflow as cascade-delete owner kind). Supported template
    constructs: `container`, `script` (simplified: `command:
    ["sh","-c", source]`), `steps` (sequential groups of parallel
    steps), `dag` layering; unsupported constructs
    (suspend/http/resource/plugin/missing template) park the workflow
    at `Pending` with a `status.message` naming them. Node model
    simplification: node ID = node name (upstream hash elided); root
    node typed Steps/DAG/Pod; StepGroup nodes `<wf>[g]`; step nodes
    `<wf>[g].<step>`; DAG tasks `<wf>.<task>`; failure → workflow
    `Failed` + message + `Completed` condition; all-succeeded →
    `Succeeded` + `finishedAt`. Pods run WITHOUT the upstream
    wait/executor sidecar — the template container runs directly and
    kubelet terminal pod phases drive node phases.
  - **As-built (Sprint 23, Scope C, Q30):** the **CronWorkflow
    controller** — new `crates/controllers/src/controllers/cron_expr.rs`
    (393 lines, pure Vixie-cron engine: 5-field expressions `*`/lists/
    ranges/steps, dom/dow EITHER semantics when both restricted,
    dow 7==Sunday, aliases `@hourly/@daily/@midnight/@weekly/@monthly/
    @yearly/@annually`, `@every <duration>` with compound `s`/`m`/`h`/`d`
    pairs e.g. `1m30s`; `next_after()` strictly-after, minute
    granularity, 366-day horizon cap; 9 unit tests) and
    `cronworkflow.rs` (353 lines, reconcile over raw `Value`):
    schedule source `spec.schedules` (v4 array) else `spec.schedule`
    (v3); `suspend` short-circuit; invalid schedule → `SubmissionError`
    condition upsert; due = any expr `next_after(lastScheduledTime`
    else `creationTimestamp)` <= now; concurrencyPolicy
    Allow(default)/Forbid(skip the tick WITHOUT advancing
    lastScheduledTime — upstream parity)/Replace(delete active children
    then fire); fires child Workflow `<cwf-name>-<unix_ts>` with
    `spec.workflowSpec` cloned verbatim + `workflowMetadata.labels`
    overlay, label `workflows.argoproj.io/cron-workflow`, ownerReference
    `CronWorkflow/argoproj.io/v1alpha1`; status `active`
    (ObjectReference list) + `lastScheduledTime` (only when a child is
    actually created), write-if-changed via semantic_eq + CAS. Wiring:
    `runner.rs` (639 lines — Kind::CronWorkflow queue + informer +
    2 workers + 5s resync + Workflow-event owner-enqueue into the cron
    queue), `controllers/mod.rs` (`Caches.cronworkflows`), `gc.rs`
    (CronWorkflow in MANAGED_OWNER_KINDS (7) + workflows in
    DEPENDENT_KINDS (6) — children cascade-delete). 4 integration tests
    in `crates/controllers/tests/cronworkflow.rs` (fires child; Forbid
    blocks + releases on terminal CAS; Replace deletes active; suspend
    never fires).
  - Documented simplifications/deviations (Scope C): no on-disk anchor →
    eager first fire (the anchor persists in `lastScheduledTime`
    thereafter); one child per due tick regardless of schedule count;
    `spec.timezone` ignored (UTC naive); NOT enforced:
    successful/failedJobsHistoryLimit, stopStrategy, succeeded/failed/
    phase status counters.
  - Future enhancements (out of TODO scope, not blockers): executor
    semantics (upstream wait container, artifacts, `{{...}}` parameter
    substitution, loops/withItems), suspend/http/resource templates,
    the argo CLI surface beyond get/apply/delete, timezone and
    history-limit handling, artifact storage and events.
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
  - As-built (Scope B): NEW **G29** — a 2-step Workflow applied via the
    built-in kubectl on a fresh SRV2 server + agent runs to `Succeeded`
    and status reports node phases (root Steps/Succeeded + both Pod
    nodes Succeeded + finishedAt), over the Q27 airgap one-shot `wfstep`
    image (`scripts/build-workflow-image.sh`, deterministic OCI tar —
    two runs byte-identical); `scripts/golden/suites/argoproj.sh` 59 →
    195 lines — 34/34 green. 4 controller integration tests in
    `crates/controllers/tests/workflow.rs` (steps-to-Succeeded,
    failed-pod-fails-workflow, container-entrypoint root Pod node, DAG
    diamond layering). kubectl `api-resources` + `api-versions` (new
    `crates/kubectl/src/discovery.rs`) close the fresh-cluster
    acceptance: live smoke on a fresh empty cluster lists `workflows wf
    argoproj.io/v1alpha1 true Workflow` and `cronworkflows cwf,cronwf
    ...` out of the box.
  - As-built (Scope C): the CronWorkflow acceptance leg is covered by
    the controller integration tests (`crates/controllers/tests/`
    `cronworkflow.rs`) — schedule firing, Forbid/Replace concurrency
    policies and suspend are exercised against the real in-process
    control loop; G28 keeps the CronWorkflow CRUD/SSA lifecycle green
    (verified live this session).

- **状态 / Status** — done (Scope A+B+C, Sprint 21-23)
- **证据 / Evidence** — Scope A: api fidelity 8 (vs 11 fixtures) /
  apiserver 8. Scope B: controllers 111 (+4 workflow integration) /
  kubelet 90 (+6 terminal-phase unit) / kubectl 60 (+11 discovery);
  golden G27/G28/G29 (34/34); Q30 ADR (+ Scope B addendum). Scope C:
  `cron_expr.rs` 9 unit + `tests/cronworkflow.rs` 4 integration;
  controllers 111 → 124; workspace **854 passed / 0 failed**;
  golden 34/34 (G28/G29); Q30 ADR (+ Scope C addendum). Clippy
  `-D warnings` zero; fmt clean.
- **卡点 / Blockers** — none; T7.3 closed at its minimal-integration
  scope (types + routes + kubectl verbs + Workflow controller +
  CronWorkflow controller + golden G27-G29). Remaining items are
  future enhancements (executor semantics, `{{...}}` params,
  suspend/http/resource templates, argo CLI breadth, timezone /
  history limits) — see the list above, not open blockers.
- **依赖 / Depends on** — T1.2, T3.1
