# init-pro Phase 4 — 100% 整体闭环规划 (S49 → S60, 目标 54/54 + 全合约对齐)

状态: **active** (S49 已执行, 2026-11-21)。本文是 Phase 4 收官层的执行计划,
与 `index.md`(TODO 状态表)、`decisions.md`(Q1-Q63)、`phase-3-host-ai.md`
(P0-P3 路径)、`CHANGELOG.md`(逐 sprint 复盘) 锁步。Phase 3 的 P0(T7.7) 已于
S49 收口; 硬件链与平台链按本文排程继续。

## 0. 「100%」的定义 — 六条全满足才算闭环

1. **54/54 TODO done**;
2. **Q1-Q63 每一条款**要么兑现、要么有 LOCKED 决策显式排除(Q59
   supported-subset 口径), 零沉默漂移;
3. **deferral 台账**(§6)每项「收口或显式弃置」二选一, 无第三态;
4. **全门禁冷机绿**(`bootstrap-link-repos → build → test → clippy →
   golden → e2e battery`, 以 `scripts/all-gates.sh` 承载);
5. **SSOT 零漂移审计**通过(含 user-owned README/CLI-about 刷新签收);
6. **真硬件证据三项落库**(T4.15 HD / T7.2 GPU6 / T7.6 INF6, Q60-Q62)。

## 1. 基线与目标态

| 项 | 基线(S48 后) | S49 后 | 目标态(S60) |
|---|---|---|---|
| TODO 计数 | 38 done / 6 in-progress / 10 not-started | **39 / 5 / 10** (T7.7 done) | **54/54 done** |
| 草案决策 | Q44/Q45/Q46/Q47 DRAFT | **Q47 LOCKED**; Q44/Q45/Q46 各留待开工前定稿 | 全部 LOCKED |
| 真硬件证据债 | T4.15(HD)/T7.2(GPU6)/T7.6(INF6) | 不变(硬件窗口未开) | 三项 recorded |
| SSOT 漂移 | README/CLI about 前置 Q32 文案(user-owned OPEN) | 不变 | 终局刷新签收 |
| 顺延包 | S47 T1.3b、S48 审计收口 → S49 | **已承接**(T1.3b 入 S50 认证链) | 清零 |

## 2. 开放 TODO 的合约对齐矩阵

| TODO | 治理合约 | 收口判据(证据) | 硬依赖 |
|---|---|---|---|
| ~~T7.7~~ | **Q63** 全条款 | ~~D7~~ **S49 已收口**: agent-runner 进程内 gated-registry 分发缝 + 节点级强制硬化开关 + Q63 逐条款复核(§7); sandbox-e2e SA1-SA8 23/23 | 无 |
| T4.15 | Q60 | node04 现场真机 HD 验收 recorded | 硬件窗口 |
| T7.2 | Q61(GPU6 强制) | per-vendor 真硬件 recorded evidence | 硬件窗口 |
| T7.6 | Q62(INF6 强制) | 真 llama-server + pin 定 GGUF + 真整卡 recorded | T7.2 收口 + 硬件 |
| T4.3 | Q34/Q41 + **Q44 定稿** | Scope C ClusterIP 分配 + L4 承载; flannel host-gw/multi-node e2e F6+ | Q44 LOCKED |
| T4.4 | Q59 边界 | netpol supported-subset + allow/deny e2e | T4.3 done |
| T4.5 | Q31 对齐 | 注册/心跳硬化 + kubelet↔server 代理隧道(exec/logs 经 apiserver) | T4.2/T1.2(done) |
| T1.3 | **Q47 定稿**(含 Q11 修订注记) | T1.3a TLS+静态 token(auth-e2e A1-A6)→ T1.3b RBAC supported-subset | ~~Q47 LOCKED~~ **S49 已定稿** |
| T3.3 | 依赖 T1.3 | bootstrap/证书/token 轮转 e2e | T1.3 done |
| T3.4 | Q18(Lease+CAS)+ Q33(etcd-gRPC client 唯一消费者) | 多 server 选主 + failover e2e | T3.3 done |
| T4.10 | Q46 供给前置 | hostPath + 经 bound PV 的 PVC 消费, volumes e2e | T6.2 done |
| T5.5 | Q32 余量 + **Q38 P2-1 pin bump** | live informer/drain/shared-dict 等价/动态证书 + TLS serving e2e | 无(可并行) |
| T5.6 | Q28/Q41 余量 | SNI/TLS passthrough e2e | T5.5 done |
| T5.7 | Q4 | Router 平台配置变量面 | T4.5 + T5.4(done) |
| T6.1 | Q59 子集 | HelmChart auto-deploy 最小面 + e2e | T1.2/T3.1(done) |
| T6.2 | **Q45/Q46 定稿**; Q22 lazy-binding 由 T6.2b 封洞 | 内嵌 Cluster DNS + local-path provisioner/binder e2e; ~~metrics-server 按 Q59 决策~~ **S49 已弃置(§6)** | T6.1 + T4.3 |

## 3. 依赖 DAG(五条链, 可并行)

```
P0 链     T7.7 D7 ──► done(S49 收口, 无外部依赖)
硬件链    T4.15 HD ─► T7.2 GPU6 ─► T7.6 INF6   (node04 现场一次性收割, 浮动可插)
认证链    Q47定稿✓ ─► T1.3a ─► T1.3b ─► T3.3 ─► T3.4
网络链    Q44定稿 ─► T4.3余量 ─► T4.4 ;  T5.5 ─► T5.6 ;  T4.5 ─► T5.7
存储链    T6.1 ─► T6.2(Q45/Q46定稿) ─► T4.10
```

## 4. Sprint 排程(S50-S60)

每 sprint 统一携带: 全门禁绿、golden 扩充、三笔提交纪律(代码+测试/证据
脚本/SSOT 锁步)、文件上限(新 ≤400/迭代 ≤800)、红线零改、Q38 pin 不动
(唯一例外: S55 的 Q38 P2-1 openrusty pin bump, 届时以决策注记记录)、
SSOT lockstep(index + plan/*.md + decisions + CHANGELOG)。

| Sprint | 内容 | 出口判据 |
|---|---|---|
| ~~S49~~ P0 收口 | ~~T7.7 D7 + 强制硬化 + Q63 复核; S48 审计收口; Q47 定稿; T1.3a 开工~~ **已执行**(§7; T1.3a 按风险裁决 1 顺延 S50) | ~~T7.7 done; Q47 LOCKED; 审计归档~~ ✓ |
| **S50** 认证链 I | T1.3a TLS+静态 token(auth-e2e A1-A6, golden h2c 基线不动)+ T1.3b RBAC supported-subset(Role/RoleBinding/ClusterRole/ClusterRoleBinding 最小 wire 兼容 + 执行中间件) | T1.3 → done;kubeconfig full parity 弃置记录(Q47) |
| **S51** 网络链 I | Q44 定稿 → T4.3 余量: Scope C ClusterIP 分配 + L4 承载(沿 Q57 折入 Router 权重面模式, 归属面定稿时裁定); flannel host-gw/multi-node F6+ | T4.3 → done; Q44 LOCKED |
| **S52** 网络链 II | T4.4 netpol(Q59 子集, allow/deny e2e)+ T4.5 代理隧道(Q31 对齐) | T4.4、T4.5 → done(解锁 T5.7) |
| **S53** 存储/addon 链 | T6.1 HelmChart auto-deploy 最小面; Q45/Q46 定稿 → T6.2a 内嵌 Cluster DNS + T6.2b local-path provisioner/binder(**封 Q22 lazy-binding 洞**) | T6.1、T6.2 → done; Q45/Q46 LOCKED |
| **S54** 卷消费 | T4.10 hostPath + PVC(bound PV 解析, 依赖 T6.2b) volumes e2e | T4.10 → done |
| **S55** Router 链 I | T5.5: Q38 P2-1 openrusty pin bump + gateway_config 前向适配; live informer/drain/shared-dict/动态证书 + TLS opt-in e2e(golden 增 TLS 基线) | T5.5 → done |
| **S56** Router 链 II | T5.6 余量 SNI/TLS passthrough + T5.7 Router 平台配置变量 | T5.6、T5.7 → done |
| **S57**(浮动) 硬件收割 | node04 现场: HD(T4.15)→ GPU6(T7.2)→ INF6(T7.6), runbook 驱动全程录像/记录 | 三 TODO → done; 窗口一到即可提前, 最晚不迟于 S59 |
| **S58** 认证链 II | T3.3 bootstrap/证书/token 轮转(建在 T1.3a 之上) | T3.3 → done |
| **S59** 认证链 III | T3.4 HA 多 server: etcd-gRPC client(唯一消费者, Q33)+ Lease/CAS 选主(Q18)+ failover e2e | T3.4 → done(54/54 若 S57 已收割) |
| **S60** 终局清算 | 54/54 核对; deferral 台账终审; done 卡内 deferred 子项清算(T4.8 命名端口、T4.9 capacity/eviction——收口或 Q59 弃置); user-owned README/README_zh/CLI-about 刷新签收; 冷机全门禁 + SSOT 零漂移审计 + 决算 CHANGELOG | §0 六条全勾 |

## 5. 风险与裁决点(按序拍板, 均不阻塞开工)

1. ~~S49 过载风险~~: 已按本表优先级执行——T1.3a 降级 S50, 其余三项 S49 硬承诺已兑现。
2. **node04 硬件窗口**是 3 个 TODO 的唯一日历依赖; 若窗口晚于 S59, S60 顺延, 不阻塞纯软件链。
3. **T3.4 是最大单体切片**(etcd-gRPC client + 集群); 若现场判定量级超限, 备选是按 Q59 记录「HA 显式延期」决策——但这与 TODO 卡冲突, 需用户裁决, 不得默认弃置。
4. ~~Q47 客户端选型~~: **S49 已拍板**(rustls 直依赖, 与 openrusty TLS 面解耦; 详见 Q47 定稿文本)。
5. Q44 的 Scope C 与 Q57 权重折入模式的交互在定稿时一并裁定(ClusterIP 分配面归 apiserver 还是 Router)。

## 6. Deferral 台账(裁决点 3 的「收口或显式弃置」二选一)

| 项 | 来源 | 处置 | 状态 |
|---|---|---|---|
| metrics-server addon | T6.2/S48 backlog | **显式弃置**: 前向支持子集外(Q59 S49 注记); HPA/`kubectl top` 若成为目标生态负载, 按 Q59 addition path 重入 | **closed (S49)** |
| kubeconfig 完整 parity | Q47 草案 | 显式弃置: T1.3a 起步仅 server/CA/token 三字段 | closed (随 Q47 定稿) |
| protobuf wire、动态 CRD 注册、admission、aggregation、全量 RBAC | Q59 | 显式弃置(Q59 原文), 随 S50 T1.3b 落 supported-subset RBAC | standing |
| T4.15/T7.2/T7.6 真硬件证据 | Q60/Q61/Q62 | **收口中**: node04 窗口收割(S57, 最晚 S59) | open |
| T4.8 命名端口、T4.9 capacity/eviction 子项 | done 卡内 deferred | S60 终审: 收口或 Q59 弃置 | open |
| Router TLS/SNI 余量、netpol、host-gw/multi-node | T5.5/T5.6/T4.3/T4.4 | **收口中**: S51-S56 排程 | open |
| HA 多 server | T3.4 | **收口中**(S59); 弃置需用户裁决, 不得默认 | open |

## 7. S49 执行记录 (2026-11-21)

**T7.7 → done(Q63 收口)。** 交付:
- **D7 opencode 进程内 tool-dispatch seam**: 新 staged binary `agent-runner`
  (`crates/agent` `[[bin]]`, 以库形式内嵌 opencoder-session; **永不**链入
  init-pro 多调用二进制——Q36 D1 libsql 防线, `cargo tree` 复核)。hardened
  attempt 的容器 argv 由 opencode CLI 改为 `/runner/agent-runner --workdir
  /workspace --policy … --audit … --attempt …`; `runner_gate::wrap_registry`
  以 `GatedTool` 包裹**全部** registry 条目(含 hallucinated 名与 `mcp__*`),
  每次分发过 `api::agent_policy::gate` — 拒绝即确定性 denial 字符串 +
  `denial_audit` JSONL(无 secret, advisory); 子代理经 execute.rs 的
  registry 逐字传递继承**同一** gate(Q63 verbatim inheritance); 广告面强制
  `ToolFilter::Allow(policy 子集)`(config/--agent 覆盖抵抗)。
- **节点级强制硬化开关**: `--agent-require-sandbox`
  (`INIT_PRO_AGENT_REQUIRE_SANDBOX`, 蕴含启用沙箱面)。开启后无
  `spec.sandbox` 的 Agent → `Pending/Ready=False/SandboxRequired`, **永不**
  落 legacy 直跑(封掉 S48 敞口)。
- **Q63 逐条款复核**: §8 表; 唯一注记项 = 真 provider 流量桥(broker op 协议
  ↔ OpenAI HTTP)仍为 stub 级证据——fresh netns 下直接出网物理不可能
  (fail-closed), 缺口是可用性而非遏制。
- **S48 审计收口**: 差距清单 C 类零无主项(§6 台账即审计产物, 每项有 TODO 卡
  或 Q 码归宿); metrics-server 按 Q59 **显式弃置**; backlog 归档入 §6。
- **Q47 定稿 LOCKED**(客户端选型 rustls 直依赖, 见 decisions.md)。
- **顺延**: T1.3a → S50(风险裁决 1, S49 容量让位 P0)。
- **门禁**: build ✓; workspace tests **1375/0**; clippy -D warnings 0;
  golden **68/68**; sandbox-e2e **23/23**(SA1-SA8, 特权真跑); flag-parity
  **27/27**; Q36 D1 复核通过。

## 8. Q63 逐条款复核表 (S49)

| Q63 条款 | 落点 | 证据 |
|---|---|---|
| 中央 default-deny action gate | `agent_policy::effective/gate` + `runner_gate::GatedTool` 运行时调用 | SA8b/c, 单测 |
| 只收窄/subagent 逐字继承 | `effective()` 求交 + `inherit()`; 子会话 registry 逐字继承 wrapped map | agent_policy_tests, runner_gate_tests |
| 所有 tool/dispatch 入口 | wrap_registry 覆盖 9 内置 + mcp__ 前缀 + hallucinated 名(Custom 类, 永不豁免); `task` spawn 入口 = 广告面剔除, 强制 spawn 落入全 gate + cgroup 限界子代 | SA8f, runner_gate_tests |
| 确定 denial + audit | gate 拒绝 → 固定字符串 + denial_audit JSONL(无 secret) | SA8b audit 形状断言 |
| Linux 隔离(私有 rootfs/fresh netns/非 root/no_new_privs/空 caps/强制 limits) | S48 已落(nsrun/engine, caps=0); S49 不变 | SA2/SA4(复跑) |
| descendant 清理 | S48 四路 teardown; S48 不变 | SA4 |
| per-attempt broker、凭证只在上游 | S48 已落; env 封闭集不变(policy/audit 走 argv) | SA3(复跑), SA5 |
| hardened 不可用 → Pending/Failed, 禁 fallback | select_sandbox fail-closed + **新增** RequiredRefused(节点强制硬化) | SA6, **SA7(新)** |
| mediation-only 不得关卡 | D7 后每分发入口均进程内强制——非 mediation-only | SA8 |
| 有限 supersede(Q35/Q37 历史) | legacy T7.1 路径原样保留; 仅 sandboxed 路径封闭 env | agent-exec-e2e 回归 |

## 9. S49 后剩余链状态

- 认证链: Q47✓ → **S50 T1.3a/b** → S58 T3.3 → S59 T3.4。
- 网络链: S51 Q44+T4.3 → S52 T4.4/T4.5; S55 T5.5 → S56 T5.6/T5.7。
- 存储链: S53 T6.1/T6.2 → S54 T4.10。
- 硬件链: S57(浮动) T4.15/T7.2/T7.6。
- 计数: **39 done / 5 in-progress / 10 not-started**。
