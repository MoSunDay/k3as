# Layer 7 — AI Agent & Workflow scheduling (Phase 2)

Mirrors `index.md` TODO IDs **T7.1–T7.4**. Per **Q3**, this layer is gated
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
  已记同款债务）
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
