# Phase 2 实施计划（控制面 + 节点 → M3 端到端集群）

> Status of this file: **active**. Tracks the Phase 2 execution plan derived
> from `index.md` / `README.md`. Created at Sprint 19 close: Sprints 10–19
> predate this file and are recorded retrospectively in `CHANGELOG.md` + the
> `index.md` sprint log; from Sprint 20 on, this file is the forward-looking
> plan — the current sprint is fully specified, later sprints are sketches
> detailed one sprint ahead. Sprint outcomes are annotated inline as they
> land. TODO IDs strictly mirror `index.md`. Phase 1 (M0 + M1) closed at
> Sprint 9 — see `phase-1-implementation.md` (archived).

## Goal（目标）
Phase 2 自 Sprint 10 起步，已按平台关键路径交付存储层（T2.1–T2.3，Q17/Q29）、
APIServer（T1.1/T1.2）、控制面循环（T3.1/T3.2）、containerd/CRI（T4.1，
Q24–Q26）、kubelet Scope A（pods 到 Running+Ready，golden G25）与 Service
NodePort 数据面（T5.6 部分，Q28）——18/33 TODO done。本文件锁定剩余关键
路径：**T4.2 Scope B/C（可信工作负载）→ T4.3（CNI）→ M3 端到端集群**。
Sprint 20 已完成（2026-08-23）；Sprint 21（T4.2 Scope C：exec/logs/attach
+ local deploy）为当前冲刺；Sprint 22+ 为路线草图。

## Scope（范围）
**纳入（关键路径优先）：** T4.2（Scope B/C）· T4.3 · T4.4 · T4.5 ·
T5.6 余量（LB VIP，受 Q28 约束）· T1.3/T3.3/T3.4（HA 分支）· T6.x
（M3 后 addons）
**机会性（非关键路径，不承诺冲刺）：** T5.5（热加载）、T5.7（Router 平台
配置变量）、T7.x（Layer 7）
**不在本文件：** Phase 1（已归档）与已 done TODO 的回顾 —— `CHANGELOG.md`
与 `index.md` sprint log 是回顾 SSOT，本文件只做前瞻。

## TODO（按 sprint 排序，每条含交付物）

### Sprint 20 — T4.2 Scope B：探针 + 卷 + 真实镜像拉取（详细规划）
> 主题：让真实工作负载可信 —— 没有探针与卷的 pod 只是"能跑"，不是"能依赖"。
> T4.2 仍为 in-progress（Scope B 记入 As-built），不提前翻状态。

**Sprint 20 于 2026-08-23 全部落地（S0–S5），门禁全绿**（workspace
717→746、golden 31/31 ×2 无 SKIP、ST1–ST6 6/6、D1–D4 4/4、clippy 0）。

- **S0 — 卫生切片（repair-on-touch）**
  - `index.md` 计数行修复（17 done → 18 done；in-progress 括注去掉
    T4.1 as-built 误列）+ `phase-1-implementation.md` 归档标注 —— 随本
    规划 commit 落地（即本文件的引入 commit）。
  - 冲刺开跑前门禁全量重跑确认基线绿：`cargo test --workspace --locked`
    （基线 717）、clippy `-D warnings` 零、golden 27/27（G24/G25 无
    vendor bundle/`cc` 时走 SKIP）、cli-flag-parity 16/16、
    service-traffic-e2e ST1–ST6、durability-e2e D1–D4。
  - ✅ S0 落地：随 67b21e5（Phase A 漂移修复）完成 —— 计数行修复 +
    phase-1 归档 + 基线 717 确认绿。
- **S1 — 探针引擎 `crates/kubelet/probe.rs`**
  - readiness / liveness / startup 三类探针；handler 支持 HTTP GET /
    TCP / exec 三种。
  - **首日 spike**：验证 Q26（route B）"crictl exec 覆盖 stdio 透传"的
    断言（Options B 原文）；失败则 exec handler 降级 HTTP/TCP-only，
    卡点记录回 T4.2（见 R4）。
  - `initialDelaySeconds / periodSeconds / timeoutSeconds /
    failureThreshold / successThreshold` 语义对齐上游 kubelet。
  - 效果接线：readiness 翻转 PodReady 条件（Endpoints 联动由 T3.1
    Endpoints 控制器自然消费，无需新线）；liveness 失败 → 重启容器
    （受 restartPolicy 约束，restartCount 经 status 上报）；startup
    通过前门控另两类探针。
  - ✅ S1 落地（`crates/kubelet/src/probes.rs` 399 行，≤400 行门禁）：
    HTTP-only `ProbeSpec`/`Tracker` 状态机 —— startup 门控 +
    readiness/liveness + liveness 失败→重启 + restartPolicy 门控 +
    Ready 派生 + `ContainerView.exitCode` 透出；exec handler 首日
    spike 未过 → 降级 HTTP-only，TCP/exec 探针顺延记入 T4.2 卡点。
    workspace 717→731。
- **S2 — 卷挂载 `crates/kubelet/volumes.rs`**
  - emptyDir：节点侧目录 `<data-dir>/pods/<uid>/volumes/emptyDir/<name>`
    （默认 data-dir 即 `/var/lib/init-pro`，见 `infra::config`），sandbox
    清理时随之删除。
  - configMap / secret → 投影为文件（key = 文件名），经 CRI create 的
    `mounts` 传进容器。
  - configMap/secret 对象变更 → sync 循环以内容指纹检测 → 原地刷新投影
    文件。
  - ✅ S2 落地（`crates/kubelet/src/volumes.rs` 400 行，≤400 行门禁）：
    emptyDir 按 volume name + configMap/secret 投影按 object name +
    FNV-1a 内容指纹原地刷新 + sandbox 清理联动；`Mount` struct +
    `ContainerConfig.mounts`（Box 修复 large_enum_variant）。
    workspace 731→741。
- **S3 — 真实镜像拉取路径**
  - kubelet 建容器前经 CRI `PullImage`（crictl pull，Q26 route B）；
    实现 `imagePullPolicy: Always / IfNotPresent / Never`（缺省规则对齐
    上游：`:latest` 标签 → Always，否则 IfNotPresent）。
  - airgap 回退沿用 Q27 模式（本地确定性 OCI tar + `ctr images import`；
    pull 失败且本地已有镜像 → 继续）。在线拉取 e2e 走 SKIP-not-fail
    门禁策略（Q24 出口阻断先例，见 R3）。
  - `imagePullSecrets`（私有仓库认证）**显式延后**：记入 T4.2 卡点，
    不纳入本冲刺。
  - ✅ S3 落地：`imagePullPolicy` 解析时默认化（latest/无 tag→Always，
    digest→IfNotPresent，显式 Never），`ensure_image` 带 policy 参数
    （sandbox 镜像恒 IfNotPresent）；`imagePullSecrets` 延后兑现
    （记入 T4.2 卡点）。workspace 741→746。
- **S4 — `PodSandboxConfig.hostname` 透传（小切片）**
  - Sprint 18 已知缺口：`crictl pods` 无 IP/host 时 ST4 用 LOCAL 作
    副本判别子即因 hostname 未透传。本切片把 sandbox config 带上
    `hostname`（v1 简化为 pod name，上游 FQDN 语义后续对齐）。
  - ✅ S4 落地：`PodSandboxConfig.hostname` = pod name 透传
    （skip-if-empty）。workspace 746。
- **S5 — golden G26 + 工作负载 e2e**
  - 扩展 `scripts/build-echo-image.sh`（或旁挂 builder）：确定性离线
    镜像带 HTTP 健康端点（如 `/healthz` + 可翻转失败端点供 liveness
    场景）——沿用 Q27 确定性构建模式（固定 mtime/排序，见 R5）。
  - golden **G26**：readiness 探针（HTTP GET）+ configMap 卷 → Pod
    Running+Ready 且卷文件容器内可读；无 vendor bundle/`cc` 时
    SKIP-not-fail（G24/G25 先例）。
  - e2e 场景：liveness 失败 → 容器重启且 restartCount 递增。
  - 门禁：全量测试 + clippy + golden 28/28（27 + G26）。
  - ✅ S5 落地：golden **G26a–d**（27 + 4 新用例 → 31/31 ×2 无 SKIP）——
    echo 镜像确定性构建新增 `/healthz`（200/500 由 `/healthz-fail`
    标记翻转）、`/fail`、`/bootcount`（`/data/boot.txt` 行数）、
    `CONFIG <file> <首行>` 行；修复 echo.c boot.txt O_WRONLY→O_RDWR
    （读回计数恒 0）。G26a 探针门控 Running+Ready；G26b /fail →
    liveness 重启 restartCount≥1 → /healthz 恢复 + Ready；G26c
    crictl stop → emptyDir 跨重启 bootcount==3；G26d configMap 投影 +
    ECHO==pod name。

### Sprint 21（当前冲刺，详细规划下一冲刺展开）— T4.2 Scope C：exec/logs/attach + local deploy
- exec / logs / attach：`kubectl logs|exec` → apiserver 透传 → kubelet →
  CRI（Q26 route B stdio；若流式订阅需求触发 route A 阈值，按 Q26 预留
  评估 feature-flag `cri-client` crate —— 100-crate 依赖代价 opt-in）。
- local deploy：语义待决（见 R2 —— 拟 k3s 式本地 manifest 自动部署目录，
  但与 T6.1 范围重叠；Sprint 21 开题拍板，本文件不预设）。
- 优雅终止取舍：Q25 kill-first（sticky shutdown）与 pod graceful
  termination（preStop / terminationGracePeriodSeconds）边界是否重开
  决策 —— 冲刺开题定。
- 交付：**T4.2 → done**（Scope A+B+C 齐）。

### Sprint 22（草图）— T4.3 CNI（依赖 T4.2 done）
- `node.spec.podCIDR` 分配（节点注册/准入路径写入）。
- flannel 等价 overlay：VXLAN vs Host-GW 取舍（单节点 v1 → 多节点 e2e
  需真实 netns 布局）。
- ClusterIP 数据面选型：iptables/nftables（kube-proxy 等价）vs Router
  扩展（Q4 延伸）——受 Q7 许可证门 + Q28（NodePort plane 已在 Router）
  约束，冲刺开题出 ADR。
- 解锁：T5.6 LB VIP 余量、T4.4（netpol）。

### Sprint 23+（草图）— 收尾 M3
- T4.5 余量：代理隧道（节点注册 + Lease 心跳已在 T4.2 Scope A as-built）。
- HA 分支：T1.3（认证授权）→ T3.3（bootstrap/证书/token 轮转）→ T3.4
  （多 server + etcd 集群；etcd-gRPC client 自 Q17 重定位后的唯一消费方）。
- T6.x addons（CoreDNS / local-storage / metrics-server 自动部署）随
  M3 验收（端到端集群）之后展开（M5）。

## Verify（验收 / Definition of Done — Sprint 20）
1. `cargo test --workspace --locked` 全绿（基线 717 + probe/volume/pull
   新增单测与集成测试）。✓ 通过：746 全绿（+29）；行数门禁
   probes.rs 399 / volumes.rs 400 ≤ 400。
2. clippy `--workspace --all-targets -- -D warnings` 零告警。✓ 零。
3. golden 28/28（新 G26；无 vendor bundle/`cc` 时 G24–G26 SKIP-not-fail
   路径可复现）。✓ 31/31 ×2（27 + G26a–d，vendored box 无 SKIP）。
4. 探针语义 e2e：readiness 翻转 Ready 条件；liveness 失败 → restartCount
   递增。✓ G26a/G26b 覆盖。
5. 卷语义：configMap key → 容器内文件可读；对象更新后投影刷新。
   ✓ G26c/G26d 覆盖（emptyDir 跨重启持久 + configMap 投影/刷新）。
6. 镜像策略：Always / IfNotPresent / Never 三态行为有测试覆盖（在线
   拉取断言走 SKIP-not-fail）。✓ 三态单测覆盖。
7. 门禁全家桶绿：cli-flag-parity、service-traffic-e2e、durability-e2e、
   multicall-selftest。✓ ST1–ST6 6/6、D1–D4 4/4、cli-flag-parity、
   multicall-selftest、bash -n 全绿。
8. SSOT lock-step：T4.2 As-built 增记 Scope B（状态仍 in-progress）、
   CHANGELOG Sprint 20 回顾、本文件 Sprint 20 节补落地标注。
   ✓ 本提交（Phase C）完成。

## Risks（风险与对策）
- **R1 — 范围超载**：S1–S5 单冲刺可能装不下 → 预约定裁剪顺序：先砍 S4
  （hostname 延后），S2 收窄为 emptyDir + configMap（secret 投影顺延
  S21）；**S1 / S3 / G26 不可裁**（探针 + 拉取路径是 Scope B 的存在
  理由）。→ **未触发**，S1–S5 全部按计划落地（2026-08-23）。
- **R2 — "local deploy" 语义未定义**：SSOT（T4.2 卡点）仅列词未释义。
  拟定义为 k3s 式本地 manifest 自动部署目录，但与 T6.1 范围重叠 →
  标注为 Sprint 21 待决项，本文件不拍板。→ 本冲刺以 hostname 透传为
  pragma 范围兑现 S4；local deploy 正式 UX 移交 S21。
- **R3 — 镜像拉取 e2e 依赖外网 registry**：Q24 已证本环境
  docker.io / registry.k8s.io 不可达 → airgap-first 设计，在线拉取测试
  走 SKIP-not-fail 门禁策略（ST/D/G24/G25 先例）。
- **R4 — exec 探针依赖 Q26 断言**（"crictl exec 覆盖 stdio 透传"）→
  S1 首日 spike 验证；失败则 exec handler 降级 HTTP/TCP-only，卡点记录
  回 T4.2，Q26 route A 触发条件提前评估。
- **R5 — G26 离线镜像需健康端点**：现 echo 镜像（`init-pro.local/echo:0.1`）
  无 HTTP 健康路径 → 需扩 builder（确定性构建，沿用 Q27 模式：固定
  mtime、零 owner、排序条目、字节稳定 tar）。

## Align（与既有规划对齐）
- 严格镜像 `index.md` 的 33 条 TODO ID 与 DAG，不改范围、不改 Q1–Q29
  既有决策（新取舍按需在 `decisions.md` 落 ADR）。
- Sprint 20 落地后 **T4.2 仍 in-progress**（Scope B done 记入
  `plan/04-node.md` As-built + 证据字段）；T4.2 → done 发生在 Scope C
  交付时（Sprint 21+）。
- CHANGELOG / `index.md` sprint log 仅在冲刺执行落地时写回顾；本文件
  是前瞻规划，冲刺落地时在对应 Sprint 节补状态标注（phase-1 先例）。
- 文档命名、结构、7 字段词汇与 `phase-1-implementation.md` 先例及
  `template/TODO.md` 风格规则一致。

## 关键路径图
```
S20 T4.2/B（探针+卷+拉取, G26）done ─> S21 T4.2/C（exec/logs/attach → T4.2 done）
                                       │
                                       v
                                 S22 T4.3 CNI ──> T4.4 netpol
                                       │
                                       v
                   S23+ T4.5 隧道 · T1.3→T3.3→T3.4 HA ──> M3（端到端集群）
                        （T6.x addons 随后；T5.6 LB VIP 余量随 T4.3 解锁）
```
