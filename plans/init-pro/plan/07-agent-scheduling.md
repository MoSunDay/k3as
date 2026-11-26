# Layer 7 — AI Agent, Workflow, sandbox & host-native inference scheduling

Mirrors `index.md` TODO IDs **T7.1–T7.7**. T7.1–T7.5 retain their completed
Phase 2 history. k3as is a multi-tenant Agent sandbox platform. One tenant per
cluster with tenant-dedicated nodes is necessary but not sufficient for hostile
multi-tenant support; the audited/tested per-tenant controls in Q60/Q63 and
`phase-3-host-ai.md` §7 are also required, and Namespaces are not boundaries. Phase 3 P0 Q63/T7.7 extends done T7.1/Q35/Q37
with Agent action interception and Linux containment without reopening it.
The independent P1–P3 inference track is Q60/T4.15 host-direct performance,
Q61/T7.2 whole-GPU scheduling, and Q62/T7.6 llama.cpp serving through ordinary
Deployment and NodePort Service objects. Host-direct is not the Agent
action-containment profile and does not create a dependency from T7.7 to the
inference track.

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

- **Phase B 实现 (Sprint 30, Q37) / Phase B implementation**
  - `kubelet::agents` 执行循环：LIST+watch agents，客户端 `spec.nodeName`
    绑定过滤同时作用于 LIST 与 watch 事件；Pending 绑定 Agent 每个一次
    尝试：runner 解析 → workspace 挂载/准备 → opencoder 子进程（argv 传参、
    凭证仅经子进程 env, Q35）→ merge-patch 回写（write-if-changed）。
  - 状态机：Pending→Running(`Executing`)→Succeeded(`Completed`, `exit 0;
    session <id>`)/Failed(`RunnerFailed`/`ExecTimeout`/`WorkspaceMountFailed`/
    `WorkspacePrepareFailed`/`RunnerMissing`)；DELETE（LIST/watch 两路）
    经 `/proc/<pid>` 杀子进程。
  - `runtime::opencoder` runner 发现/staging：env override（缺失即终态不回
    退）→ staged `<data>/bin` → embed → link_repos 祖先目录（exe/cwd 两级）。
  - workspace 源：注解 `nfs://addr/agent/<name>` → `--anyhome-addr` → 节点
    本地 `<data>/agent-ws/<name>`；挂载父导出 `/agent`（Q37 协调注记 1：
    mountd 冻结 boot 时刻目录树）；mount 源串 host-only（Q37 协调注记 2：
    `host:port:/path` 会被 mount.nfs 原样发成 dirpath `port:/path`）。
  - golden agent boots（G24-G30/nscascade/workloads）钉 `--disable-agent-exec`
    （Q37 协调注记 3：link_repos runner 不得翻转 golden 用例）。
  - e2e：`scripts/agent-exec-e2e.sh` S1/S2+G1-G7（真实挂载 + 真实子进程，
    teardown umount 不泄漏挂载）。

- **As-built addendum (Sprint 36, Q42):** the engine modules
  (`workflow_plan/dag/phase/status/pod/agent/cron_expr.rs`) and the
  typed argoproj port moved OUT of k3as into
  `link_repos/arust-workflow` (pin `687ed325`, 8th Q38 pin) as
  `workflow-model` + `workflow-engine`; k3as keeps a one-file re-export
  shim (`crates/api/src/argoproj.rs`) and a thin `Kube` bridge
  (`controllers/workflow_bridge.rs`) — behavior and wire format
  unchanged (JSON-only, Q10). See decisions.md **Q42**.
- **As-built addendum (Sprint 37c, 双节点实机验收 hardening):**
  `agent_exec.rs` 挂载韧性 — (a) pinned per-address NFS 根挂载在 anyhome
  server 重启后 stale（NFSv3 hard 不自报错, stat 挂死 → `create_dir_all`
  冒 EEXIST → 后续 Agent 全 `WorkspaceMountFailed`, F10）: 修复 =
  EEXIST 容忍 + 有界 `read_dir` 活性探针 + 纯函数 `plan_root`
  (Mount/Reuse/Recover; Recover = `umount -l` + remount) + 3 单测;
  (b) F6 首挂/重挂后 prepare 偶发 EIO → `create_dir_retry` 有界重试。
  实机: run4 复现 → run5 `ACCEPTANCE_RESTART=1` 15/15 全绿。详见
  CHANGELOG Sprint 37c (F6/F10)。
- **状态 / Status** — done (Phase A Sprint 26 + Phase B 节点执行 Sprint 30,
  Q35/Q37)；B2 专用 CLI run 动词不再单列（`init-pro agent` 子命令 + kubectl
  apply 已覆盖创建路径，需求再现时并入后续 TODO）
- **验收 / Acceptance** — `cargo test --workspace --locked` 973 passed /
  0 failed（Phase B 新增 kubelet agents_runner_integration 6 项：本地回退全
  生命周期、RunnerMissing、RunnerFailed、ExecTimeout、DELETE 杀子进程、
  绑定过滤回归；opencoder 单测 7 项）；`scripts/agent-exec-e2e.sh`
  9 passed / 0 failed；`scripts/golden-conformance.sh` ALL passed；
  `scripts/cli-flag-parity-test.sh` 19/19；`scripts/anyhome-e2e.sh` 4/4；
  clippy `--workspace --all-targets -- -D warnings` 清零
- **债务 / Debts** — 状态回写走 merge-patch（status 子资源合并语义为 Q34
  已记同款债务）；**F4 (S37c 兜底加固, 已闭环)**: 初判「执行循环按 name
  去重 → 同名重建永不 re-reconcile」为代码缺陷，后续证伪（修复前二进制
  上删除→同名重建仍 Succeeded；去掉 uid 的单测变体亦通过）。按对象身份
  语义**加固**处理：写回缓存判重键由 name 改为 `name#uid`
  (`crates/kubelet/src/agent_exec.rs` `dedupe_id`；`AgentView` 增
  `uid`)，同名重建不被前代 last-body 去重，另有单测 + 集成回归
  (`recreated_same_name_agent_reconciles_again`)。收尾（S37c 追加二，
  commit `6d0ef7d`）：可达机制 = 早期失败分支（EmptyPrompt/
  WorkspaceMountFailed/WorkspacePrepareFailed）终态体先于 Running 写回
  且跨化身恒等 → name 键压制同名重建重写；成功路径因 Running 体中转
  自愈（解释原复现失败）。补两个可判别集成测试
  （`early_failure_same_name_recreate_rewrites`/
  `runner_missing_same_name_recreate_reports_again`）+ 负向检查（name
  键下两测均 20 s 超时；uid 键 9/9，kubelet lib 6/6）；
  `report_missing_runner` 判重键 uid 化（`dedupe_id` 共享）。实机
  （node04 `7845e459…`）空 prompt 同名重建双化身 `Failed/EmptyPrompt`；
  验收 run9 **13/0**。残余候选工单（勿标已修）：`killers` name 键删→建
  竞态可致重复 spawn（Q37 at-least-once 容忍，但浪费进程/争用
  workspace）；`patch_if_changed` 先记后发 — 早期失败路径一次 patch 失败
  即永久压制；写回缓存无淘汰，DELETED 不清理。见 plan/04 遗留发现 F4。
- **依赖 / Depends on** — T3.2, T1.2

---

## T7.2 — 宿主 GPU 直调调度 (builtin host-direct first; Q61)

- **目标 / Goal**
  用标准扩展资源 `nvidia.com/gpu` 与 `amd.com/gpu` 为普通推理 Pod 提供整卡、
  逻辑独占的宿主 GPU 调度。首片只支持 tenant-dedicated 集群内的 builtin +
  T4.15 host-direct 性能画像；不依赖 T7.1/T7.7 Agent，不引入外部
  device-plugin，也不承诺 containerd GPU 支持。

- **核心实现 / Core implementation**
  - 只有兼容的 builtin host-direct 节点（builtin engine + `--builtin-rootfs=host`
    + host-direct 与 device capability checks 均成功）可发布非零 `nvidia.com/gpu`/`amd.com/gpu`
    capacity/allocatable；containerd、image-rootfs、probe 失败或 incapable 节点即使
    有物理设备也必须 omit 或同时报零。GPU Pod 仍须 literal `init-pro.io/rootfs: /`，
    节点资格不放松 Q60 三重选择。capacity/allocatable 是当前 eligible 整卡总量；
    assigned-Pod 用量仍由 scheduler/allocation ledger 记账。
  - canonical Node status construction 始终接收当前 eligible GPU inventory，并用于
    registration、**每次 heartbeat**、agent restart 和每次 full status rebuild；当前
    heartbeat 会替换完整 status，不得使用丢弃 GPU 字段的另一条路径。inventory/
    capability/backend transitions 后不得残留 stale nonzero allocatable。
  - 修正 T3.2 通用资源语义：Patch JSON Pointer 对 `~`/`/` 分别转义为
    `~0`/`~1`；调度快照计入所有已 assigned 且仍占用资源的 Pod；扩展资源
    request/limit 必须为正整数且相等。调度失败必须报告 Insufficient/不支持
    原因，不能把未知容量当成无限容量。
  - 节点侧持久化分配账本以 **Pod UID + container name** 为键，记录 resource、
    物理 device ID 与生命周期。retry/agent restart 先恢复同一分配，再接纳新
    Pod；以原子预留防止 double booking，teardown 完成后才释放设备。
  - NVIDIA/AMD 分别注入 `CUDA_VISIBLE_DEVICES`/`HIP_VISIBLE_DEVICES`，选择
    已分配的整卡。host-direct 或 GPU 请求若落在不支持的后端，必须显式失败/
    Pending，不得回退 containerd（无论是否静默）。
  - 首片明确排除外部 device-plugin、containerd GPU/device-plugin、分数 GPU、
    MIG、time-slicing、Agent 专属 GPU 接入及通用调度策略面；这些均 deferred，
    不是隐藏的部分实现。

- **验收手段 / Acceptance**
  - 单测覆盖 JSON Pointer 转义、absent capacity=0、assigned Pod 求和、整数且
    request=limit 校验、重复/终态 Pod 计数和分配键稳定性；非法 resources 必须拒绝。
  - 默认 CPU CI 的 **fake subscopes** 使用 fake inventory：一张 fake GPU 上第二个
    Pod 必须 Pending；同 UID/container 在重试与 agent restart 后得到同一 device/env；
    并发创建无 double booking；实际 teardown 后才释放并允许下一 Pod。覆盖
    registration、每次 heartbeat、agent restart、每次 full rebuild 的 canonical
    status 构造，以及 inventory/capability/backend transitions 后无 stale nonzero
    publication；不兼容后端、资源不足与非法 resources 均清楚失败且零 containerd
    fallback。
  - 真实硬件在普通 CI 可 opt-in，但 recorded real-hardware evidence 是关闭整个
    T7.2 的强制条件：每一个声称支持的 vendor 都须验证 inventory、可见设备 env、
    整卡互斥、restart recovery 与 teardown release；若声称 NVIDIA+AMD 则两者都要，
    否则明确收窄支持范围和 Q59 matrix。全 fake green 而无真机证据时本卡保持
    not-done；hardware-required 模式缺硬件/驱动必须 FAIL，不得以 fake 替代。

- **状态 / Status** — in-progress (S47 fake-scope 落地: `kubelet/src/gpu{,_/}` — `--gpu-inventory auto|none|fake:<res>:<n>` 清单 (nvidia-smi/rocm-smi/sysfs best-effort, 仅 host-direct-capable 节点发布), `engine/gpu-ledger.json` Pod-UID×container 持久整卡账本 (tmp+rename 原子写, 重启稳定复用, teardown 完成才释放, grace 防泄漏), 调度器扩展资源 assigned-pod 核算 + GPU1 指针转义修复, CUDA/HIP_VISIBLE_DEVICES 稳定注入, `status.initProRuntime` (gpuLedger+released) 节点心跳发布, containerd 回退硬拒绝; GPU1-GPU5 单测 + fake e2e IF7; GPU6 真硬件 per-vendor recorded evidence 未有——T7.2 保持开放)
- **证据 / Evidence** — planned fake-inventory CI + mandatory recorded run for every
  supported hardware vendor (opt-in in normal CI); no implementation or hardware
  evidence yet.
- **卡点 / Blockers** — 真实 NVIDIA/AMD 验收需要操作者预装驱动和硬件；
  `*_VISIBLE_DEVICES` 只做逻辑选择，不是 Agent 或跨租户安全边界。节点必须
  专属单一租户；Agent action containment 由独立的 Q63/T7.7 提供。
- **依赖 / Depends on** — T4.15, T3.2

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

- **状态 / Status** — done (Scope A+B+C, Sprint 21-23; engine+model
  extracted to the standalone arust-workflow repo in Sprint 36, **Q42**)
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

---

## T7.4 — anyhome 一级公民 + workspace 自动挂载 (内嵌 NFSv3 记忆面; Q36)

- **目标 / Goal**
  Workflow/Agent 有了真正的记忆面：`link_repos/any-home`（nfsserve，NFSv3 +
  MOUNT + portmap 复用在 ONE 端口）作为 path-dep 内嵌进 `init-pro server`
  并默认自启（`--disable-anyhome` 退出）；workspace 的三问 —— 服务端把字节
  暴露在哪、容器挂到哪、哪个对象拥有哪棵子树 —— 由同一个 SSOT 回答
  （`crates/common/src/workspace.rs`），keeper / 控制器注入 / Phase B 执行
  三方从此无法漂移。Agent 实际执行仍留 Phase B（Q35 不变，见下）。

- **核心实现 / Core implementation**
  - **workspace SSOT** `crates/common/src/workspace.rs`：`MOUNT_TARGET =
    "/workspace"`（容器内唯一挂载点，Agent/Workflow 一视同仁）、
    `agent_workspace_path(id) = /agent/{id}`、
    `workflow_workspace_path(id) = /workflow/{id}`、
    `workspace_nfs_url(addr, path) = nfs://{addr}{path}`。id 守卫只拒绝
    `/`（唯一路径显著分隔符；id 是 k8s metadata.name，`.` 合法）。纯模块：
    无 I/O、无 env、无状态。
  - **内嵌装配** `crates/cli/src/anyhome.rs`：逐层镜像上游
    `link_repos/any-home/crates/anyhome/src/main.rs` —— `RealFs` 覆盖
    `<data-dir>/anyhome/fs`（启动即预建 `workflow/` + `agent/` 根）、
    `MemoryDb` 落在 `<data-dir>/anyhome/anyhome.db`、`DummyEmbedder`
    取上游 `--dim` 缺省 64（`EMBED_DIM` 由单测解析上游 `Cli` 锁死：
    MemoryDb reopen 校验 dim，漂移会在构建期报错而不是损坏磁盘库）、
    export `/`。nfsserve 没有 cancel seam：serve loop 是
    `tokio::select!{handle_forever, Shutdown}`；优雅停机前对 MemoryDb 做
    WAL checkpoint（上游三臂日志，checkpoint 失败不丢数据——写路径已
    fsync）。启动失败降级为 WARN（workspace 面劣化，不 fatal）。
  - **workspace keeper** `crates/cli/src/anyhome_keeper.rs`：server 持有
    storage `Arc`，keeper 直接轮询 store（Q19：不走 loopback HTTP、无
    token），为每个 Workflow/Agent 预建 `<fs>/workflow|agent/<id>`，容器
    先于第一次写挂载也能拿到目录。**删除被刻意忽略（A5）**：记忆面要字节
    比对象活得久，GC 需要 owner/subresource 模型，属上游协同而非 keeper
    职责。与 `flannel_keeper` 同款拆分理由：两个文件都守 400 行上限。
  - **server 接线**：`--anyhome-addr`（缺省 `127.0.0.1:11111`）、
    `--disable-anyhome`，env `INIT_PRO_ANYHOME_ADDR` /
    `INIT_PRO_DISABLE_ANYHOME`；`anyhome_endpoint()` 纯函数把
    disabled/addr 折叠成 `Option<String>` 端点，`None` = NFS 面不上线，
    `Some` 原样透传（wildcard/port-0 继续可用）。
  - **Workflow step Pod 自动挂载** `workflow_pod.rs::build_pod_with`：
    `ManagerConfig.workspace_nfs`（与 `cluster_cidr` 同款线程化：runner →
    wiring → worker → `workflow::reconcile(client, &wf, workspace_nfs)`）
    非空时给 step Pod 追加卷 `{"name":"workspace","nfs":{server=addr,
    path=/workflow/<wf-name>,readOnly:false}}` + 主容器
    `mountPath: MOUNT_TARGET` 的 volumeMount（`container` 与 `script` 两类
    模板共用同一主容器，均覆盖）。**注入豁免（A4：用户声明优先）**——
    用户已写 `mountPath == /workspace` 或已声明名为 `workspace` 的卷
    （workflow 级或模板级）时不注入；`--disable-anyhome` → endpoint 为
    `None` → 逐字节回到 pre-Q36 的 `build_pod` 形状。
  - **Agent 契约注解** `workflow_agent.rs`：`build_agent_with` 在
    `workspace_nfs` 就位时为 Agent 子对象打
    `init-pro.io/workspace = nfs://<addr>/agent/<agent-name>`
    （`WORKSPACE_ANNOTATION`）。这就是 Phase B 执行 + kubectl 的契约面；
    Phase A 只发契约不执行。
  - **kubelet 侧**：`nfs` 卷类型（k8s `NFSVolumeSource`）消费同一形状，
    细节见 `plan/04-node.md` T4.2 的 Sprint 27 as-built 块。
  - **D1 约束（构建期硬边界）**：SSOT 落在 `common` 而非 `agent` ——
    `agent` 经 opencoder-store 牵 libsql 0.9.30，anyhome 自带 libsql
    0.10.0-pre.4，两份符号（`update_hook_cb`）在同一二进制内链接冲突；
    二进制依赖路径上的任何 crate 不得依赖 `agent`。

- **验收手段 / Acceptance**
  - 单测：common workspace 路径/守卫/URL 8 条；cli 装配 + endpoint 折叠 +
    keeper 同步/守卫；controllers 注入/豁免/两类模板/无 endpoint 不注入 +
    Agent 注解有/无；kubelet nfs 卷解析、挂载参数、幂等与 umount-before-rm。
  - **真机 e2e `scripts/anyhome-e2e.sh`**：**A1** 根挂载往返
    （`mount.nfs 127.0.0.1:/` 写读一致且落盘）、**P2** 子路径挂载
    `host:/workflow/<name>`（上游未知的 A2 疑问——本机实测**可用**，无需
    上游协同；informational）、**K1** POST Workflow 后 keeper ≤15s 建目录
    （controllers 关闭也成立：keeper 读裸存储，不依赖 reconcile）。
  - golden conformance 的 discovery/路由字节不受本 TODO 影响；非
    containerd 用例（G01–G23、G31–G33）全程全绿，containerd 生命周期用例
    在本共享沙箱受负载抖动（见证据行与 CHANGELOG Sprint 27 的偏差记录）。

- **状态 / Status** — done (Sprint 27, Q36)；Agent 执行留 Phase B (Q35 不变)
- **证据 / Evidence** — workspace **927 passed / 0 failed**（基线 882
  (Sprint 26 收口; wave 前 852) + 45 新增）；`scripts/anyhome-e2e.sh`
  A1 PASS / P2 PASS / K1 PASS；golden 非 containerd 用例全绿，containerd
  生命周期用例（G25/G26/G29/G30）在共享沙箱高负载下超时抖动（最佳完整跑
  28 过 / 8 失败，失败集逐轮漂移、同一二进制复现不稳定——环境性，非代码
  回归；独立复跑同一流程 pod 可达 Running+Ready）；flag parity
  19/19（含新 `live_help "t-anyhome"` 行）；clippy `-D warnings` 0
- **卡点 / Blockers** — `DummyEmbedder` 使 `/search` 只做精确匹配（真实
  语义检索需上游 embedder）；A5 删除/GC 需 owner 模型（见上，非本 TODO
  阻塞）；D1 令 Agent 执行无法进 server 进程 —— Phase B 必须走节点侧
  （B1）/ 新子命令（B2），与 Q35 记录一致
- **依赖 / Depends on** — T7.3, T4.2
- **S54 rebase 注记 (Q66, 2026-11-25)** — anyhome 实体下线, 本 TODO
  语义与验收口径不变: `link_repos/any-home` (RealFs/MemoryDb/
  DummyEmbedder/anyhome.db) 整体退役——其检索层 (MemoryDb/libsql、
  `/search` RPC) 在 init-pro 内零消费者, 无可观察损失; 平面底座换为
  nfsserve 直连 + 内嵌 DirFs (`crates/cli/src/anyhome_vfs.rs` +
  `anyhome_dirfs.rs`, adapted from nfsserve `examples/mirrorfs.rs` @
  c00c361, BSD-3-Clause, 见 NOTICE)。上方「内嵌装配」小节的 any-home
  逐层镜像描述为历史 as-built; 现行装配 = DirFs →
  `NFSTcpListener::bind` 单口 + 导出 `/` (serve 循环 `tokio::select!`
  + Shutdown drain 姿势不变)。用户面契约 (flags/env/盘上布局/keeper/
  注入链/kubelet 消费面) 零改动——A1/P2/K1 验收原样复跑通过 (S54:
  anyhome-e2e 7/0)。D1 约束保留但 rationale 改写为 link-graph hygiene
  (Q66)。详见 decisions.md Q36 supersede 注记 + Q66。
- **长稳清扫注记 (2026-11-26, B12)** — server drain 中 anyhome NFS 平面的
  join 加 `ANYHOME_DRAIN_TIMEOUT` 10 s 上界 (超时 abort + WARN, 不再让一个
  挂住的 NFS 连接拖死整个 drain); 用户面契约不变。

## T7.5 — anyhome Pod 级注解挂载 (init-pro.io/anyhome + anyhome-mounts; Q43)

- **目标 / Goal**
  把 anyhome 记忆面从 "Workflow step Pod / Agent 专属" 放开为 **任意 Pod 可注解启用**:
  `init-pro.io/anyhome: rw|ro|true|false` 打开 per-Pod 工作区(服务器路径
  `/pod/<namespace>/<pod-name>` → 容器内 `/workspace`, `ro` 即卷 `readOnly: true`);
  `init-pro.io/anyhome-mounts: "<服务器路径>:<容器路径>[:ro|:rw],..."` 每段生成一个
  独立 nfs 卷 + volumeMount(任意多段)。A4 用户优先: 容器已声明同挂载点、或 Pod 已有
  同卷名/同 nfs 路径的卷则跳过注入(与 Workflow 注入同型, 天然不打架)。

- **核心实现 / Core implementation**
  - `crates/common/src/workspace.rs` (SSOT): 注解键常量、`pod_workspace_path(ns,name)`
    → `/pod/<ns>/<name>`(额外 `pod/` 根隔离——ns 可合法命名 `workflow`, 字面
    `/<ns>/<name>` 布局会与既有 `/workflow`、`/agent` 根冲突)、`parse_anyhome_mode`、
    `parse_anyhome_mounts`(绝对路径校验、拒 `.`/`..`/空段、按容器路径去重首段胜)、
    `anyhome_mount_volume_name`(`anyhome-<净化容器路径>`, RFC1123 ≤63, 确定性)。
  - `crates/apiserver/src/pod_workspace.rs` (NEW, 400 行): `prepare(workspace_nfs,
    &mut body)` 纯 JSON 手术, 幂等(卷名/nfs 路径/挂载点名判重)。注入点 =
    `do_create` + `do_apply` **每次 apply**(SSA 会删除 manager 拥有但新配置缺失的
    字段, 仅 created 分支注入会在同 manager 下次 apply 时被剥离; pre-merge 保持
    恰一份 owner 一致副本)。非法注解值 → 422 Invalid。`--disable-anyhome`
    (`AppState.workspace_nfs == None`) 完全惰性: 不校验、字节零变化。接线:
    `AppState.workspace_nfs` + `serve()` 加参 + `api_app_with_workspace_nfs()`
    (旧 `api_app` 保持 3 参委托 None, 存量测试零改动); `cli/runtime.rs` 传
    `ServerBind.anyhome`。
  - keeper (`crates/cli/src/anyhome_keeper.rs`): beat 增加第三页 LIST pods, 按
    注解过滤, `mkdir -p` 预建 `/pod/<ns>/<name>` 与各自定义服务器路径
    (`anyhome.rs::prepare_fs_roots` 同步建 `pod/` 根); 删除继续忽略 (A5)。
  - **post-review hardening (Sprint 36b, T7.5a/T7.5b)**: `resolve_volume_name`
    解决净化同名碰撞(`/A_B` 与 `/a-b` 同得 `anyhome-a-b` → 第二卷曾静默丢失、
    挂载错绑第一卷路径): 同 nfs 路径 → 别名复用既有卷名(一卷多挂载点; 引擎
    `workspace` 卷绑定真名不再悬空); 基名空闲 → 基名; 基名为传入体所有 → A4
    绑定不改名; 否则按注解顺序 `-2`/`-3` 确定性消歧。挂载点判重改为仅按
    mountPath。`do_replace` 接入同一 `prepare` 钩子(整替不再剥卷); 身份守卫仅在
    注解存在时生效。initContainers 刻意不注入、无 per-path authz —— 信任模型
    见 decisions.md Q43 post-review 段。
  - kubelet **零改动**: 通用 `nfs` 卷 + `readOnly`(卷级与挂载级 OR) Sprint 20
    已就绪; `ro` 语义 = 客户端强制(CRI readonly bind), 与原生 k8s NFS readOnly
    同信任模型; 引脚(nfsserve/any-home)零 patch。

- **验收手段 / Acceptance**
  - 单测: common 解析器 8 例(拼写/遍历拒绝/去重/卷名确定性); apiserver
    `pod_workspace` 10 例(rw/ro/mounts/A4 豁免/双跑幂等/None 字节不变/422)+ 集成
    `tests/anyhome_pod_mounts.rs` 8 例(201 后 GET 断言卷+挂载形状、ro readOnly、
    多段、裸 Pod 无 volumes 键、plane-off 惰性、SSA 二次 apply 不重复、A4 用户胜);
    cli keeper pods 维度 3 例(提取/幂等落盘/删除忽略)。
  - 真机 e2e `scripts/anyhome-e2e.sh` 新增 **P3**(rw 注入断言 + 裸 Pod 字节稳定 +
    keeper ≤15s 建目录)、**P4**(ro → readOnly:true)、**P5**(双自定义路径卷+挂载+
    keeper 目录); curl/python3 缺失时按 K1 同款降级 doctrine 退出必需集。
  - 无注解 Pod 字节零变化 → golden conformance (T0.6) 不受影响(实测全绿)。

- **状态 / Status** — done (Sprint 35, Q43; Sprint 36b post-review hardening; Sprint 54 Q66 底座更换零触碰——注入链/keeper/e2e P3-P5 原样复跑)
- **证据 / Evidence** — `cargo test --workspace --locked` **991 passed / 0 failed**
  (Sprint 35: common +8 / apiserver +18 / cli keeper +4; Sprint 36b hardening:
  apiserver +5 单测 +2 集成); `cargo clippy --workspace
  --all-targets -- -D warnings` 0; `scripts/anyhome-e2e.sh` **A1/P2/K1 + P3/P4/P5
  7 passed / 0 failed**; `scripts/golden-conformance.sh` **73 passed / 0 failed,
  exit 0**(83486ad 树复跑; 早前同树 66/7 为沙箱环境根因 — 残留宿主 cni0
  网桥 10.42.1.1/24 令 flannel 沙箱全败(G25/G26), 高载争用令 G23 的 6s
  二号 server boot 窗口超时, 均非代码回归) — 根因明细见 CHANGELOG
  Sprint 35 gate note。
- **卡点 / Blockers** — Pod 目录无 GC (A5 延续, 需 owner 模型, 高频重建会累积
  目录); keeper 全量 LIST pods 的轮询开销(小集群 v1 可接受, 后续可换 watch);
  readOnly 为客户端强制(服务器端 per-path ro 需 nfsserve vfs 能力, 不做)。
- **依赖 / Depends on** — T7.4, T1.2

---

## T7.6 — llama.cpp-first 宿主推理 (ordinary Deployment + NodePort; Q62)

- **目标 / Goal**
  用普通 `apps/v1` Deployment 产生 Pod，再以普通 `core/v1` NodePort Service
  暴露一个 llama.cpp `llama-server`。首片在一租户一集群且节点专属该租户的
  部署中，直接复用 T4.15/T7.2 的普通推理 host-direct 整卡能力；**不是 Agent，
  不新增 CRD，不新增自动模型部署控制器**，也不以 host-direct 代替 Q63 Agent
  action containment。

- **核心实现 / Core implementation**
  - 提供可审计的 Deployment/Pod/Service 清单契约：Pod 显式选择 builtin +
    `--builtin-rootfs=host` 节点和 `init-pro.io/rootfs: /`，使用标准 GPU request/
    limit。当前 Recreate 不足以保证单 GPU 安全：前向实现须先将 old RS 缩零，再将
    new RS 保持 zero，直到所有旧 containers/sandboxes 的实际 runtime teardown 和
    GPU allocation release 均明确 acknowledge/complete。RS spec/status zero、
    deletionTimestamp、terminating state 或 API Pod absence 均非完成证明。机制由
    实现决定，但必须表示 node/runtime + allocation ledger completion，并关联旧
    Pods/attempts 使 stale ack 不能解锁；stall 时 replacement 保持 zero 且 status
    可操作，仅在完成后继续。非法 Deployment `strategy.type` 必须拒绝，不得按
    RollingUpdate 处理。
  - `llama-server` 必须由宿主预装并钉定版本；GGUF 必须由 operator 预先放到
    宿主文件系统，作为不可变模型经 `init-pro.io/host-mounts` 只读挂载。缺
    binary/model 或校验失败即清楚报错；运行面绝不隐式下载模型或工具。
  - 直接暴露 llama-server 的 OpenAI 兼容接口：`GET /v1/models`、
    `POST /v1/chat/completions` 的 non-streaming JSON 与 streaming SSE；
    `GET /health` 作为 readiness probe。Service 使用现有通用 NodePort 数据面，
    Router 不解析模型、不增加 model-aware routing。
  - 默认 CI 以 fake inventory + fake OpenAI server 验证 API/生命周期；真实 pinned
    llama-server + 本地只读 GGUF + real GPU e2e 可在普通 CI opt-in，禁止网络下载。
    fake green 只能关闭明确标注的 fake subscopes；整个 T7.6 done 必须有 recorded
    real run，证明 health/models/chat/streaming 及 actual accelerator execution。

- **验收手段 / Acceptance**
  - CPU/fake e2e 用真实 `kubectl`/`kube-rs` 应用普通 Deployment + NodePort Service，
    验证 inference Pod 的 Service/Endpoints 生命周期：无 ready backend 时 endpoints
    为空，Ready 后经 NodePort 验证 health/models/chat non-streaming/streaming SSE；
    既有 G70/G71/ST1-ST6 仅作通用数据面证据，不重复 generic proxy tests。断言无
    Agent/新 CRD/模型下载/model-aware Router 配置。
  - Recreate 验收注入 stalled teardown/release，断言 old RS 缩零后 new RS 持续 zero、
    stale ack 无效且 status 可操作；仅在旧 containers/sandboxes 实际拆除且 ledger
    release 完成后继续。非法 Deployment strategy 必须被 Kubernetes-style validation
    拒绝，不能落入 RollingUpdate。
  - 真机 e2e 仅使用预装 pinned llama-server、本地只读 GGUF 和已分配真实 GPU，
    重复 health/models/chat/streaming 检查并证明 actual accelerator execution。
    普通 CI 缺前置条件可明确 `SKIP`，hardware-required 模式必须 FAIL；但 recorded
    real run 是整个 T7.6 done 的强制条件，全 fake green 时本卡保持 not-done。
  - T0.6 supported-subset golden、常用 workspace gates 与 T4.15/T7.2 回归全绿；
    fake 通过不计作真实 llama.cpp/GPU 验收证据。

- **状态 / Status** — in-progress (S47: controllers `teardown_hold` — Recreate 先缩零 old RS, new RS 保持 0 直到旧 host-direct/GPU Pod 的 runtime teardown + GPU 账本释放被当前节点 `status.initProRuntime` (released 时间戳晚于 `init-pro.io/teardown-since`, 且 gpuLedger 无该 UID) 显式确认, stale ack 不可解锁, 等待期 Deployment Progressing=False/OldRuntimeTeardownPending; 离线 fake 面: `scripts/inference-fake-e2e.sh` (python3 fake OpenAI, 禁下载) INF1/2/3/5+IF7/IF8 11/11 PASS——Endpoints 空→ready、/health readiness、models/chat/SSE [DONE] 过 NodePort、CUDA_VISIBLE_DEVICES 注入、无模型感知路由、无镜像拉取; ops 交付: `plans/init-pro/ops/` node04 盘点模板+切换/回滚 runbook+参数化 Deployment/NodePort 清单; INF6 真 llama-server/GGUF/整卡验收未做——T7.6 保持开放)
- **证据 / Evidence** — planned fake OpenAI CI + mandatory recorded pinned
  llama-server/read-only GGUF/real-GPU run (opt-in in normal CI); none yet.
- **卡点 / Blockers** — 真机验收依赖 operator 提供兼容驱动、GPU、pinned
  llama-server 与足够的本地 GGUF 存储；模型分发/GC、多模型路由和自动部署
  控制器均 deferred。
- **依赖 / Depends on** — T7.2, T3.1

---

## T7.7 — Agent action interception + Linux sandbox (Q63)

- **目标 / Goal**
  扩展已完成的 T7.1/Q35/Q37，在 Agent 可执行任意工具、task/subagent 或外部
  provider 操作之前，增加节点/operator 所有的 default-deny action policy、
  统一拦截与 Linux 强制隔离；**不重开 T7.1，也不修改其历史证据**。本卡负责
  Agent action containment。一租户一集群且节点专属该租户是 hostile multi-tenant
  support 的必要但非充分条件，还须 Q60/Q63 与 Phase 3 §7 所列的 per-tenant
  网络、存储、端点和凭证隔离经审计/测试，否则该部署不受支持。Namespace 不是
  租户安全边界，Q60 host-direct 也不是 containment。优先复用现有 Linux/nsrun
  能力，不另造第二套 runtime/daemon/syscall/container stack。

- **核心实现 / Core implementation**
  - **明确 staged scopes。** mediation-only central gate/provider broker 仅是开发
    milestone，不是 Q63 containment 或受支持的 hardened mode，不能关闭 T7.7；
    containment 不可用时 hardened request 必须 Pending/Failed。hardened completion
    要求 Agent、opencoder、所有 ordinary/custom/MCP/ssh tools、所有 subagents 和
    descendants 同受一个 action gate，并同处一个 attempt 的 Linux identity/
    namespaces/cgroups/cleanup，无一逃逸。按适用边界复用 nsrun/clone3/cgroups/
    engine lifecycle，不新增第二套 runtime/daemon/syscall/container stack。
  - **策略所有权与收窄。** default-deny baseline 仅由 node/operator 管理；
    AgentSpec、workspace config、resumed sessions、model/agent changes 与
    subagents 只能进一步 narrow，绝不能 broaden、替换或禁用 baseline。
    对配置覆盖、恢复会话或切换模型/agent 后的有效策略重新求交；试图扩权
    必须得到确定性 denial 并产生 audit event。
  - **中央 action gate。** 所有 ordinary tools、最早的 task/subagent dispatch、
    MCP、ssh 与 custom tools 都必须先经过同一个 tool-dispatch gate，不能经
    early dispatch 绕过。subagent 继承与父 Agent **完全相同**的有效策略，
    其后续 tool/subagent 仍回到同一 gate； denied action 返回稳定、可测试的
    denial result，并记录不含 secret 的 audit event。
  - **复用 Linux 隔离。** 基于 done T4.14 的 nsrun/clone3/cgroup 工作启动每次
    Agent attempt：私有 **非宿主** rootfs + mount namespace、fresh network
    namespace、非 root uid/gid、强制 `no_new_privs`、空 capability sets。仅允许
    operator-allowlisted selective host binds；runtime/tool binds 只读，仅指定
    workspace/scratch 可写，Agent config 不得扩大。`/dev` 只能是 fresh minimal
    safe nodes，绝不暴露宿主 `/dev`。host container/engine/Docker/CRI/其他
    runtime-control sockets 均不可见；per-attempt broker Unix socket 是唯一有意的
    host IPC exception。禁止 literal host `/`、host netns 和 Q60 host-direct。
  - **强制资源与后代清理。** cgroup v2 CPU、memory、PIDs limits 均为必填且
    有界，不能省略成 unlimited；将 Agent/opencoder/tool/subagent 全部纳入同一
    受控生命周期，attempt 结束、失败或超时后可靠清理全部 descendants，再
    回收 cgroup 与临时挂载。
  - **provider broker。** provider 访问只能经 node-side、operation-allowlisted
    broker；每个 Agent attempt 使用独立 Unix socket，attempt 结束即失效。
    broker 只允许配置的 provider operations，并在上游连接侧注入凭证；它不是
    generic proxy，不开放任意 egress。Agent、opencoder、ordinary/custom/MCP/
    ssh tools 与 subagents 的环境均不得获得 provider credentials。
  - **Q35/Q37 关系与失败语义。** Q63 仅对 sandboxed Agent path supersede
    Q35/Q37 的 child-env credential transport；Q35/Q37 的完成状态、历史实现与
    证据保持不变。sandbox 任一强制能力不可用时，Agent 必须保持 Pending 或
    Failed 并给出可操作 reason；禁止 direct-process、host-root、host-network
    或 containerd fallback。T7.7 独立于 T4.15/T7.2/T7.6。

- **验收手段 / Acceptance**
  - 可分阶段交付，但 mediation-only gate/broker 不是 containment，不能关闭本卡。
    unit/fake tests 覆盖 default-deny、policy inheritance/narrowing，以及 AgentSpec/
    workspace/resumed-session/model/agent/subagent 的 override resistance；逐一覆盖
    ordinary tool、early task/subagent、MCP、ssh、custom tool，断言 subagent 策略
    逐字等效、denial 确定性及 audit event。
  - 特权 Linux integration 验证非宿主 rootfs/mount、fresh netns、非 root/空 caps/
    `no_new_privs`、强制 cgroup v2 CPU/memory/PIDs limits；覆盖 allowed/denied
    binds、runtime/tool 只读、workspace/scratch 外写入拒绝、fresh minimal `/dev`、
    host container/engine/Docker/CRI/其他 runtime sockets 不可见，以及 Agent/
    opencoder/tools/subagents/descendants 同 gate/identity/namespaces/cgroups/
    cleanup。成功/失败/超时/取消均清理 descendants；宿主 `/`、host netns、宿主
    `/dev` 与 Q60 host-direct 不可达。
  - provider 验收只使用本地 fake provider + per-attempt Unix-socket broker，
    验证 operation allowlist、非 generic proxy、socket 生命周期及上游凭证注入；
    secret canaries 必须不出现在 Agent/opencoder/tool/subagent 的 env、argv
    或 logs/audit 中。不得依赖任何外部 service。
  - 注入 sandbox/broker 不可用条件，断言 Pending/Failed reason 可操作且四类
    fallback 均未发生；containment 与 credential confinement 均 fail closed。
    T0.6 supported-subset golden、现有 workspace build/test/clippy/safety 与
    Agent/engine 回归门保持全绿。

- **状态 / Status** — done (S48 Stage A 全量 + Stage B 执行面落地: `api/src/agent_policy.rs` (+tests 拆分) — AgentSpec `policy` 只收窄 overlay + `sandbox` (mode:"hardened", 强制 limits) wire 类型、operator-owned default-deny `PolicyBaseline`、纯函数只收窄求交 `effective()` (扩权 => 确定性 `PolicyViolation` + audit)、subagent 逐字 `inherit()`、中央 gate 判定 `gate()` + 无 secret `denial_audit()`, apiserver 准入臂 ("init-pro.io","Agent") 经 `validate_agent_spec`; `engine-proto` `ContainerSpec.caps` (serde default = Docker 14-cap `DEFAULT_CAPS` 掩码, 与 nsrun lock-step 单测, sandbox 发 0); `kubelet/src/agent_broker.rs` per-attempt Unix-socket broker (仅 loopback 上游、operation allowlist、凭证只注入上游、attempt 结束吊销); `agent_sandbox{,_plan}.rs` fail-closed `select_sandbox` + 纯 `sandbox_spec()` + `validate_hardened()`; `agent_exec_sandbox.rs` (+`engine/agent_stream.rs`) hardened 分支先于 legacy T7.1 路径——封闭集 env (继承 env 整体丢弃, Q63 在该路径 supersede Q37 child-env 凭证传输)、exit0/exit7/超时/DELETE 四路 verdict 共享一个 teardown; opencoder 进程内 S49 D7 收口——opencode 进程内 tool-dispatch seam 落地为独立 staged 二进制 `agent-runner` (`crates/agent` `[[bin]]`, 绝不链接进 init-pro 多调用二进制, 保住 Q36/D1 libsql 不变式): `runner_gate` GatedTool 以默认拒绝策略包裹 native 分发点 (含 hallucinated 与 `mcp__` 名), 父 registry 逐字传入子会话, `task` spawn 在广告层拦截; 节点级 `--agent-require-sandbox` 强制硬化开关 (fail-closed Pending + `SandboxRequired`, 禁 host-direct/containerd 伪成功), T7.7 全量 done)
- **证据 / Evidence** — `scripts/sandbox-e2e.sh` (NEW, 特权 Linux 本机真跑: root + cgroup v2 + 静态 busybox, **无 SKIP**) AS1-AS6 **16/16 PASS**——SA1 策略确定性、SA2 uid/gid 1000 + CapEff 全零 + NoNewPrivs + 私有 rootfs/netns/minimal `/dev` + 无 containerd.sock、SA3 broker socket 生命周期 + fake provider 日志空 (无任意 egress)、SA4 容器内 memory.max/pids.max/cpu.max + 四路后代清理、SA5 secret canaries 不入 env/logs、SA6 无沙箱配置节点 fail-closed Pending/SandboxUnavailable; golden 新套件 `sandbox.sh` **G99-G106** (golden-conformance **68/68**: hardened 往返、soft/缺限/uid0-gid0/坏 tool/相对与 `..` binds 拒绝、legacy Agent 仍准入、SSA 保持); workspace tests **1364/0** + clippy 0; 回归 engine-selftest 33/33、cli-flag-parity 26/26、inference-fake 11/11, all-gates battery 已收编 sandbox-e2e。S49 追加证据: sandbox-e2e 扩至 SA1-SA8 23/23 PASS——SA7 require-sandbox 拒绝面 + legacy-stub tripwire、SA8a-f 真实 agent-runner probe (gated 分发/子会话继承/策略文件透传); workspace tests 1375/0。
- **卡点 / Blockers** — 完整隔离验收需要支持 cgroup v2 的特权 Linux 宿主
  (S48 已在本机 root + cgroup v2 特权真跑一次; 未来复跑仍需同类宿主)；
  不得以缺少外部 provider、真实凭证或网络服务作为测试依赖，也不得用
  host-direct/containerd 结果冒充 sandbox 证据。
- **As-built (长稳清扫 2026-11-26, C5/C6/C7/C14 + H1–H6)** — attempt
  生命周期加固 (gate/策略语义不变): `agent_exec_sandbox.rs`
  `TeardownBudget` (drain 5 s / bye 1 s / reap 5 s) + `Attempt` Drop
  kill+reap; `agent_sandbox_plan.rs` per-attempt 目录 + 开机
  `sweep_stale_attempt_dirs`, `retry_delay` 500 ms 起倍增封顶 5 min;
  `agents.rs` `SlotGuard` / `PendingBackoff` (pending Agent 不再每轮重
  转)。`agent_exec.rs`: 独立进程组 `kill_group` + 输出尾部 `OUTPUT_CAP`
  64 KiB。`agent_broker.rs`: accept 致命错误分类 + 50 ms 重试, conn 读
  10 s / 写 5 s, 上游 connect 3 s。`engine/launch.rs`: engine 日志 1 MiB
  封顶, stdin 管道 + `--exit-on-stdin-eof` (kubelet 死则 engine 自退)。
- **依赖 / Depends on** — T7.1, T4.14
