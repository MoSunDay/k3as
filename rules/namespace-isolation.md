# 规则卡: clone3 只隔离 pid/uts/ipc/mnt/cgroup, 不默认隔离 network 与 fs

**决策码**: Q48 (T4.11 引擎形态) · Q49 (T4.12 进程池/netns join) ·
Q52 (T4.13 host netns) · Q53 (overlayfs) · Q60 (T4.15 host-direct) ·
Q63 (T7.7 Agent sandbox)。
**权威实现**: `crates/nsrun/src/lib.rs` 的 `clone_flags(&RunSpec)` (纯函数,
T4.11/T4.15)。本卡复述该函数与 `RunSpec` (`crates/nsrun/src/spec.rs`) 的
既有语义, 不得与之冲突。

## 1. 硬规则 (违反即错误)

1. **每个容器进程的 clone3 基础 flag 恒为五个 namespace**:
   `NEWNS | NEWPID | NEWUTS | NEWIPC | NEWCGROUP` (mnt/pid/uts/ipc/cgroup)。
   这五个不可裁剪、不可按 profile 减配。
2. **`CLONE_NEWNET` 绝不出现在默认 flag 里**——网络隔离是**显式 opt-in**:
   仅当 `spec.new_net && spec.netns.is_none()` (要求新建 netns) 才追加。
   缺省 = **留在宿主 network namespace** (Q52 host-net); `spec.netns`
   (Q49) = 加入既有 pod sandbox netns (`/proc/<pid>/ns/net`, 由引擎
   sandbox 持有, CNI/flannel 网络以 join 方式接入)。
3. **文件系统隔离同样是显式 opt-in, 不是 clone3 的默认效果**: fs 隔离由
   `rootfs` 目录 + `enter_rootfs/pivot_root` (+ 可选 overlay, Q53) 实现;
   `host_root` (Q60) 路径则**根本不做** rootfs 切换。
4. **host-direct (Q60) 画像 = 用户口径的「只隔离 namespace/pid」**:
   - 选择条件三者同时成立, 缺一不可: `--container-engine=builtin` +
     `--builtin-rootfs=host` + Pod annotation `init-pro.io/rootfs: /`;
     `rootfs` 必须是字面量 `/` (spec.rs `validate_host_root` 强校验)。
   - clone3 flag 恒为五个基础 namespace, **无 NEWNET** (宿主网络保留,
     pause/sandbox 子进程同样不加, lib.rs 测试钉死 HD3)。
   - **不隔离 fs**: 子进程保留宿主 mount table 的**私有 NEWNS 副本**作为
     `/` (只隔离 mount propagation, 宿主 mount table 原样)——无
     pivot_root、无 overlay、无新鲜 `/sys`/`/dev`, 仅在私有副本内把新 PID
     namespace 的新鲜 `/proc` 盖在宿主 `/proc` 之上。
   - 互斥 (违反即 422/拒绝, 非静默降级): `host_root` 与
     `new_net`/`netns`/`overlay` 互斥。
   - 该路径**不**发出 `Action::EnsureImage`、不 pull/不拷贝/不
     pivot `/`; `image` 仅为 wire metadata。宿主 `/dev` 原样保留
     (accelerator 直访, Q60)。
   - 边界: 仅 tenant-dedicated 集群内**普通推理**的性能画像; 不是 Agent
     containment、不是跨租户边界; **Agent 工作负载必须走 Q63**, 不得选
     此路径; 无 containerd fallback (显式或静默均禁止)。
5. **sandbox 画像 (Q63, Agent 动作遏制) 与 host-direct 相反**, 不得混用:
   私有**非宿主** rootfs + **全新** network namespace + 非根 uid +
   `no_new_privs` + 空 capability + 强制 CPU/memory/PID 限额 + 子孙进程
   清理。contour 不可用时 hardened 请求 Pending/Failed, 不得伪装成功。
6. `clone3(child_stack = 0)` 要求调用方**单线程**: 唯一调用方是
   `init-pro engine serve` 单线程进程 (Q48); 不得在 tokio daemon/多线程
   上下文直接调 nsrun 的 clone 路径 (`exec_into` 除外, 它走 setns)。
   `scripts/engine-singlethread-check.sh` 是门禁。
7. `crates/nsrun` 是全仓唯一 unsafe 岛 (`#![forbid(unsafe_code)]` 之外的
   显式豁免, NOTICE 声明); 新增 unsafe 必须留在该 crate 并过
   `scripts/unsafe-audit.sh`。

## 2. profile 对照表

| profile | NEWNET | rootfs/fs | 网络位置 | 触发 | 决策 |
|---|---|---|---|---|---|
| host-direct (普通推理) | **否** | 字面量 `/`, 宿主 mount 私有副本 | 宿主 netns | 三重显式选择 | Q60 |
| Agent sandbox (T7.7) | **是** (全新) | 私有非宿主 rootfs | 新建 netns | Agent + hardened | Q63 |
| builtin Pod sandbox (T4.12/T4.13) | 否 (join) | rootfs 目录 + pivot (+overlay) | join CCI/CPI netns 或 host (Q52) | `--container-engine=builtin` | Q49/Q52/Q53 |
| 单发 smoke (`engine-run`) | 可选 `--new-net` | rootfs 目录 | 可选新建 | CLI flag | Q48 |

## 3. 修改纪律

- 动 `clone_flags`/`RunSpec` 字段语义前: 先在 decisions.md 修订对应
  Q 码 (Q60/Q63 的条款是 LOCKED), 再动代码; 新增互斥项必须在
  `spec.rs::validate_*` 显式校验并有单测 (参考
  `clone_flags_add_newnet_only_for_a_fresh_netns` /
  `validate_host_root` 的钉法)。
- 引擎/调度侧只做**显式选择 + 拒绝矛盾输入** (可执行报错, 参照
  `crates/engine/src/convert.rs` HD5 的 actionable error 风格),
  不得静默降级到另一画像。
