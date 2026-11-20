# Layer 4 — Node / Agent (built-in engine track)

Mirrors `index.md` TODO IDs **T4.11–T4.15**: the built-in container
engine (kernel P1 Q48, kubelet wiring P2 Q49-Q51, images + netns P3
Q52-Q54, host-rootfs default Q55, literal host-direct execution Q60).
Split out of `04-node.md` at T4.14 (file-size limit). T4.15 extends the
completed engine track; it does not reopen T4.11–T4.14.

---

## T4.11 — 内置容器引擎内核 P1 (clone3/namespace, 无 runc/containerd)

- **目标 / Goal**
  自研容器引擎的 P1：用 Linux 原语直接 clone/隔离/exec 容器，提供单线程
  engine 进程与内部帧协议；不依赖 runc/containerd 也能跑起一个进程。
  决策 Q48；来源 kiri `crates/kiri-init/src/linux/` @ `7846fe0`
  (Apache-2.0)。

- **核心实现 / Core implementation**
  - `crates/nsrun` (NEW；**唯一允许 unsafe 的 crate**，Q48):
    `clone3(child_stack = 0)` 进 `CLONE_NEWNS|NEWPID|NEWUTS|NEWIPC|
    NEWCGROUP|NEWNET` + `CLONE_INTO_CGROUP`；child 顺序
    `MS_PRIVATE` → bind rootfs → enter → `/proc`/`/sys`/`/dev` → masked/ro
    `/proc/sys` → `pivot_root` + `umount2(MNT_DETACH)` → hostname →
    cap-bounding drop → optional `no_new_privs` → `setgid`/`setuid` →
    `execve`；parent start gate + `waitpid`，`128+WTERMSIG`，
    exec 失败 `126/127/255`。ro bind 用 `mount_setattr(2)`
    (`AT_EMPTY_PATH|AT_RECURSIVE`, `RDONLY|NOSUID|NODEV|NOEXEC`)，
    `mount_setattr` 支持纳入 preflight；cgroup v2 `init-pro/<id>` +
    退出清理；preflight 要求真 root + Linux 5.12+；`spawn` 前检查
    `/proc/self/status` 的 `Threads: 1`。
  - `crates/engine-proto` (NEW): `ContainerSpec` + `Request`/`Event` +
    `u32 LE len | u8 kind | payload` 帧 (kind 1 JSON, 2/3/4 raw stdio；
    `MAX_FRAME` 16 MiB)。
  - `crates/engine` (NEW): 单线程 `engine-run` 一次性冒烟动词 +
    `engine serve --socket` accept loop (无 tokio/threads)；preflight
    失败显式报错供上层回退 containerd。
  - `crates/engine-client` (NEW): daemon 侧 tokio Unix socket 客户端 +
    事件泵 (P2 kubelet 接线用)。
  - `crates/init-pro/src/main.rs`: clap 之前拦截 `engine`/`engine-run`
    (剥掉前导 `engine` token)；`crates/multicall` 增 `Action::Engine`
    别名。
  - `--container-engine=containerd|builtin` (env
    `INIT_PRO_CONTAINER_ENGINE`, 默认 `containerd`) 落 CLI；P1 仅接受 +
    告警，不改变运行时选择。

- **验收手段 / Acceptance**
  - `scripts/unsafe-audit.sh`: unsafe 只在 nsrun；其余 crate 根
    `#![forbid(unsafe_code)]`；nsrun unsafe 均带 `SAFETY:`；lint 门已开。
  - `scripts/engine-singlethread-check.sh`: engine 依赖闭包无 async/
    thread/spawn；engine 依赖仅 engine-proto+nsrun；pre-clap 派发在位。
  - `scripts/engine-selftest.sh` E1–E8 (22 断言, 真 root)：退出码、
    126/127、uid/gid/env/hostname、PID 1 + cap 边界 + no_new_privs、
    ro/rw bind、cgroup 限制与清理、`--new-net`。
  - `scripts/builtin-runtime-e2e.sh` B1–B7: ping/pong、run 事件序 +
    stdout/stderr + code 7、exec 失败 127、协议层 cgroup、非 UTF-8
    原始字节、坏请求 error+close、shutdown bye + 进程 0。
  - `cargo test -p nsrun -p engine-proto -p engine -p engine-client` 绿；
    clippy `-D warnings` 0。

- **状态 / Status** — done (P1; P2 拆分到 T4.12，P3 到 T4.13)
- **证据 / Evidence** — `crates/nsrun/src/{lib,sys,child,mount,inroot,caps,cgroup,spec}.rs`,
  `crates/engine-proto/src/lib.rs`, `crates/engine/src/{lib,cli,convert,server}.rs`,
  `crates/engine-client/src/lib.rs`, `scripts/{unsafe-audit.sh,engine-singlethread-check.sh,engine-selftest.sh,builtin-runtime-e2e.sh}`,
  `NOTICE`, `LICENSES/Apache-2.0.txt`。
  - **测试 (S38c 收尾复跑)** — engine 栈
    `cargo test -p nsrun -p engine -p engine-proto -p engine-client`
    **41 passed / 0 failed** (nsrun 22 / engine 12 / engine-proto 5 /
    engine-client 2)；`cargo test --workspace --locked` **1050 / 0**；
    `engine-selftest.sh` E1–E8 **22 断言 / 22 PASS**；
    `builtin-runtime-e2e.sh` B1–B7 全 PASS。
  - **已验证的运行语义 (S38c)** — 单连接 / 一次只跑一个容器 (单线程、
    无 tokio、无 `std::thread`)，运行期间第二个客户端收到
    `Event::Error("engine busy: one container at a time")`；握手读超时
    30 s + 写超时 30 s；SIGTERM/SIGINT/SIGHUP 均被捕 → 杀容器 + 回收 +
    删 cgroup (容器运行中实测 205–215 ms，空闲 11–14 ms)，退出码 0；
    child 侧 `PR_SET_PDEATHSIG = SIGKILL` (凭据切换后重新 arm)，engine
    被 SIGKILL 时容器 ~5 ms 随之倒下 (该路径 cgroup 目录泄漏——已无进程
    可清理)；活 engine 的 socket 从不 unlink (同路径第二个
    `engine serve` exit 1，陈旧 socket 才被替换)。`engine-run` 侧:
    `--help` exit 0；`--cgroup` 未配 `--memory-max`/`--cpu-max`/
    `--pids-max` 任一即硬错误；过 start gate 之后的容器侧失败以
    **125** + fd 2 消息上报 (127 = exec 目标不存在，126 = EACCES/EISDIR)。
- **卡点 / Blockers** — none
- **依赖 / Depends on** — T4.1 (CRI 形状), T0.1 (multicall 派发)

---

## T4.12 — 内置引擎 kubelet 接线 P2 (`EngineBackend` + exec + 真实选择)

- **目标 / Goal**
  kubelet 能把 Pod 生命周期跑在 builtin engine 上（Q49 进程池模型：
  每容器一个 `engine serve`，sandbox = pause engine），并由
  `--container-engine=builtin` 经 Q51 boot 探针真实选择；不经过
  containerd/crictl。

- **核心实现 / Core implementation**
  - `crates/kubelet/src/engine/` (NEW 目录，非单文件): `backend.rs`
    (`EngineBackend` 实现 `CriBackend` 11 方法；状态串
    `SANDBOX_READY`/`CONTAINER_RUNNING`/`CONTAINER_EXITED`)、
    `registry.rs` (id→socket/pid/规格；重启后扫
    `<dd>/run/engine/` 恢复)、`launch.rs` (起 sandbox=pause engine +
    容器 engine；wait 收割映射 `Exited`)、`logs.rs` (与 kubelet 读取侧
    同构的日志路径计算)、`exec.rs` (连容器 socket 发 `Exec`，供
    `server.rs`/`runner.rs` 直调面)。
  - `nsrun` 基座: `RunSpec` 增 `netns`/`pause`；child 对
    `/proc/<sandbox_pid>/ns/net` `setns`（先于 pivot_root）；pause 模式
    过 start gate 后永久等待信号（Q49 sandbox）；`Child::signal(signo)`
    （SIGKILL 直发）；`nsrun/src/execin.rs` (NEW): fork-relay exec 进
    运行中容器的 mnt/ipc/uts/net/cgroup(+pid) namespace + chroot 阶梯
    （PID ns 需 fork-relay，>1 天探针则按停止规则降级）。
  - 协议扩展 (`engine-proto`): `ContainerSpec` 增 `netns`/`pause`/
    `log_path`；`Request` 增 `Signal{id,signo}`、`Exec{id,argv,env,
    workdir}`；`Event` 增 `ExecStarted{exec_id,pid}`、
    `ExecExited{exec_id,code}`。exec 单在途，kind 2/3 复用；**kind 4
    双侧维持拒绝**（Q50）；engine 侧 `logs.rs` (NEW) 以 O_APPEND 写
    CRI 文本行（Q50 路径）。
  - 选择 + 回退: `--container-engine` 值真实解析（`builtin|containerd`）；
    builtin 分支跳过 vendor staging/监督，起 Q51 boot 探针
    （`_probe.sock` + 15s ping 预算，失败 kill+wait+unlink 后回退
    containerd 并 Warning）；builtin 时 node runtimeVersion 报
    `engine://<version>`；幂等删除复用 tolerate() 文本匹配。
  - 移除 `--container-engine=builtin` 的 "not wired" 告警；P1 的
    `engine-run`/`engine serve` 保持调试入口。

- **验收手段 / Acceptance**
  - Fake-engine 单测（沿 `tests/support/mod.rs` 模式）: 11 个
    `CriBackend` 方法 + 事件映射 + attempt 递增（max+1 规则）。
  - `scripts/engine-selftest.sh` 增 **E9**（sandbox pause: pause 引擎
    ping 带 pid、`--netns` 加入验证同 netns）、**E10**（运行中容器
    exec: ps 可见、合并流往返、退出码透传）、**E11**（signal → 容器以
    `128+WTERMSIG` 退出）。
  - `scripts/builtin-runtime-e2e.sh` 扩 B8+: signal/exec/pause/日志
    落盘 + 选择翻转 + 探针失败回退 + engine socket 永不复用
    `containerd.sock`（`runtime/src/lib.rs` unlink 冲突面）。
  - `scripts/all-gates.sh` 全绿（battery 8.x 现有行）。

- **状态 / Status** — done (Sprint 41; Q49/Q50/Q51)
- **证据 / Evidence** — 提交 `db12408` (nsrun exec 基座) → `b2f61ba`
  (engine 协议扩展 + pause/exec/signal + CRI 日志落盘) → `034dab1`
  (kubelet EngineBackend) → `8d2c383` (cli builtin 选择 + Q51 探针)。
  落点: `crates/nsrun/src/execin.rs`、`crates/engine/src/{exec,logs,session,wire,server}.rs`、
  `crates/engine-proto/src/lib.rs` (Signal/Exec/ExecStarted/ExecExited +
  netns/pause/log_path)、`crates/kubelet/src/engine/*` (backend/registry/
  launch/logs/exec/pump/spec)、`crates/cli/src/{engine_probe,storage_dsn,runtime}.rs`。Q48 允许清单
  修正在 Sprint 41 收口: engine 内联 `civil_from_days`/`rfc3339_nano`
  (logs.rs, 与 `common/time` 逐字同步), Cargo.toml 回到 engine-proto
  + nsrun 两依赖。tests 全绿: nsrun 28 / engine-proto
  2 / engine 22 / engine-client 7 / kubelet ~121 / multicall 20 / common
  15 / runtime 53; clippy -D warnings 0。e2e: `engine-selftest.sh`
  E1–E11 26/0; `builtin-runtime-e2e.sh` B1–B13。验收面按实测内核语义
  修正: pid-ns init 仅在装有 handler 时才收到祖先 ns 的 SIGTERM
  (SIGKILL/SIGSTOP 例外; E10→137 证明 128+WTERMSIG 映射), E11/B8
  温和停止走 trap 路径; exec 面落在协议脚本 B6/B7 (原计划 E10);
  探针失败回退由 `engine_probe` 单测覆盖; e2e 级注入 Sprint 43 收口裁定不再排期 (Q51 决策面已单测覆盖, 'N4' 名已被 Sprint 42 网络面占用——见 CHANGELOG Sprint 43)。
- **依赖 / Depends on** — T4.11, T4.2

---

## T4.13 — 内置引擎镜像存储 + Pod 网络面 P3

- **目标 / Goal**
  builtin engine 能拉取并运行真实镜像 (内嵌 registry / Docker Registry
  v2)，并给 Pod 一个确定的网络面，形成 containerd-free 闭环。两面均已
  定案收口：网络面 **Q52** (host netns 共享)；镜像存储 **Q53/Q54**
  (层目录共享 + per-container overlayfs 视图, tar/flate2/zstd 解包)。

- **核心实现 / Core implementation**
  - **网络面 (Sprint 42, Q52)**：builtin 模式下所有 Pod 共享宿主
    netns——pause 沙箱 `engine serve --pause --host-net` 不再
    CLONE_NEWNET (nsrun 零改动)；worker 照旧
    `setns /proc/<pause_pid>/ns/net` (= 宿主 netns)；
    `EngineBackend` 存 `node_ip`，`list_sandboxes` 对 SANDBOX_READY
    视图置 `ip = --node-ip`，`build_pod_status` 落
    `hostIP = cfg.node_ip` → podIP == hostIP == node IP；agent 在
    builtin 下跳过 flanneld (containerd 路径 Q34 不变)。端口冲突由
    用户自负 (hostNetwork 同语义)；`--node-ip` 默认 127.0.0.1 仅
    单节点正确，多节点需显式传真实 IP。
  - **镜像存储 (Sprint 43, Q53/Q54, M1-M5)**：真实 registry-v2
    拉取链路 (`engine/image/{pull,reference,manifest,config}.rs`:
    manifest list 平台选择、digest 逐 blob 校验、单飞 single-flight;
    Docker PascalCase + OCI lowerCamel config 双态解析) → 节点侧
    content-addressed CAS (`cas.rs`: blobs/refs/merged/tmp + 原子
    提交) → tar/gzip/zstd 解包到共享层目录 `layers/<digest>`
    (解包一次, 遍历防护 + 双 whiteout 语义: 内核 overlay 路径
    char 0:0 mknod + `trusted.overlay.opaque`, fused 兜底逻辑删除)
    → per-container overlayfs 视图 `views/<id>/{upper,work,merged}`
    (`probe.rs` 一次性可用性探测, 不可用退 fused-copy)。镜像 config
    即真源 (`spec.rs merge_launch`: entrypoint/cmd/env/user/cwd 合并)。
    GC = mark-sweep (ref 文件即根集合; `recover()` 启动清扫 +
    `remove_image`); 配额软帽 `--builtin-image-quota` 拉取前预检;
    拉取凭证仅环境变量 (`INIT_PRO_PULL_AUTH[_<MUNGED>]`, Q35 D1
    同源不落盘), imagePullSecrets 推迟; 默认 `--registry-addr`
    对接 T6.3 内嵌 registry。
  - 明确 out (Q52/Q53)：CNI/veth/网桥、netpol/egress、跨节点
    overlay；镜像构建、跨节点镜像分发协议、Windows 镜像。
  - 默认引擎 go/no-go (Q51 遗留, Sprint 43 裁定)：**不翻转**——默认
    仍 `containerd`，builtin 保持 opt-in (`--container-engine=builtin`;
    多节点 flannel/netpol 面 T4.3 仍深绑 containerd, 翻转另立决策)。

- **验收手段 / Acceptance**
  - 单测：digest 校验、平台选择、`parse_ref` 矩阵、`merge_launch`
    合并矩阵、unpack 白盒 (遍历防护 + 双 whiteout)、GC/usage。
  - e2e：`builtin-runtime-e2e.sh` **N1-N5** (battery 8.5) +
    `builtin-image-e2e.sh` **I1-I8** (battery 8.13) — I1/I2 拉取+
    失败隔离、I3 双层+whiteout 共享解包+overlay、I4 entrypoint 即
    命令、I5 env 合并 logs/exec、I6 NodePort 30000 + 端口冲突、
    I7 启动 GC、I8a CAS 持久化不重拉、I8b 配额干净失败。
  - `scripts/all-gates.sh` (golden + battery 10 门 + 8.13) 全绿。

- **状态 / Status** — done (Sprint 42 网络面 Q52; Sprint 43 镜像
  存储 M1-M5, Q53/Q54)
- **证据 / Evidence** — Stage A `56276a8` (review 收尾: --host-net/
  --netns 互斥 + runtime.rs 拆 stage_cmd.rs)；M1 `8b7d20c`
  (decisions Q53/Q54 + `engine/image/{mod,manifest,config}.rs`)；
  M2 `7412728` (http.rs 流式 GET + `cas.rs` + `pull.rs`/`reference.rs`
  + `--registry-addr`)；M3 `4a2efe4` (`unpack.rs`(+tests)/`layers.rs`
  /`gc.rs`/`probe.rs` + `OverlaySpec` 全链路 overlay)；M4 `f06b448`
  (`spec.rs merge_launch` + STOP_GRACE 注解 + exec env 继承)；
  M5 `7df9db8` (配额软帽 + 启动 GC + 环境变量凭证 + `remove_image`)。
  测试 `cargo test --workspace --locked` **1131 / 0**；clippy
  `-D warnings` 0；golden 73/0 (g24 registry.k8s.io 离线 SKIP, Q25)；
  builtin-image-e2e I1-I8 双跑全绿；runtime-e2e + parity 23/23。
- **卡点 / Blockers** — none
- **依赖 / Depends on** — T4.12, T4.3, T6.3

## T4.14 — 内置引擎宿主根文件系统 (host-rootfs 默认 + host-mounts 注解)

- **目标 / Goal**
  builtin 引擎默认根文件系统从「必须拉镜像」翻转为「宿主目录直通」:
  `--builtin-rootfs host|image` (默认 host)。host 模式以宿主 rootfs 为 `/`
  (NEWNS 挂载表 = 唯一可见性边界), Q53/Q54 镜像链降为注解 opt-in (代码
  保留)。Pod 注解 `init-pro.io/host-mounts` 提供宿主目录挂载
  (`container-path=host-path[:ro|:rw]`, Q43 同构), `init-pro.io/rootfs:
  image` 在 host 引擎下一次性拉镜像。

- **核心实现 / Core implementation**
  - `kubelet/src/engine/rootfs.rs` (新): `RootfsMode` + `parse`/`resolve_mode`
    + staged 解析 (`<data_dir>/engine/images/<name>` → legacy
    `<data_dir>/images/<name>` 兜底 → 可操作错误列补救); CAS 控制名
    ({blobs,refs,layers,views,merged,tmp}/dotfile/多段名) 由 `staged_name_ok`
    拒绝。
  - `kubelet/src/host_mounts.rs` (新): 注解解析/校验 (空项容忍; 容器路径
    重复拒绝; 与 spec volumeMounts 碰撞 = pod 级拒绝, 该周期阻断所有
    create) + `emit_error_events` (CreateContainerConfig Warning)。
  - `engine/backend.rs`: mode 字段 + `with_rootfs_mode`; resolve/present/
    list 感知 staged 名; host 模式 EnsureImage = presence-only 永不拉;
    `create_container` 注解翻转一次性拉; `image_config_body` 仅 image 模式
    (host: pod spec 即启动真源, 无 entrypoint/cmd 融合)。
  - `sync.rs`: plan 级 host-mounts 门控 (同 env_block 形态) +
    `container_cfg` 追加 `cri_mounts` + `emit_action_error_events`
    (Failed Warning; EnsureImage 按镜像 ref 回配 pod)。
  - `exec.rs`: CreateStartContainer 前挂载源存在性预检 (缺失 =
    bind 本可裁决, 预检只为可操作事件); `runner.rs`: 双事件 pass 去重
    (`last_host_events`/`last_action_events`)。
  - CLI: `agent --builtin-rootfs` (default `host`, env
    `INIT_PRO_BUILTIN_ROOTFS`), 贯穿 runtime.rs (`run_agent`/
    `run_supervised`/`run_server` 内对齐) + lib.rs agent arm +
    flags/conflicts 测试; `EngineBackend` 应用 mode (warn-fallback)。
  - nsrun 零代码改动 (命名空间集 NEWNS|NEWPID|NEWUTS|NEWIPC|NEWCGROUP
    锁定, Q55); 仅新增 spec 校验单测 (readonly×uid 正交交叉 + pause
    uid 锁不变)。

- **验收手段 / Acceptance**
  - 单测: rootfs.rs 4 项 + host_mounts.rs 3 项 + nsrun 正交交叉 1 项
    (Q55 权限姿态: 可见性 = 挂载表, 写 = uid/gid + caps)。
  - e2e: `scripts/builtin-hostroot-e2e.sh` **H1-H8** (battery 8.14):
    H1 绝对路径/staged 名双拼法、H2 宿主根标记不可见 (NEWNS)、H3 rw 挂载
    回写宿主可见、H4 `:ro` EROFS exitCode=1 (restartPolicy Never)、H5
    缺失挂载源 → Pending + log + event、H6 缺失 rootfs 名 → Failed event
    (含 stage/annotate 提示文案)、H7 生命周期 + logs + exec + /workspace
    (mount.nfs 条件)、H8 复跑 builtin-image-e2e I1-I8 (`--builtin-rootfs
    image` 下回归)。已知限制: rootfs=`/` 字面量不支持 (nsrun 切根前 sysfs
    挂载 EBUSY, Q55) —— H7 改用 fabricated rootfs + host-mounts
    `/workspace=$ws:rw,/host=/:ro` (MS_BIND|MS_REC 目录绑定只读带出宿主树)。
  - `scripts/all-gates.sh` 全绿; cli-flag-parity 增 t414 行。

- **状态 / Status** — done (Sprint 44, Q55)
- **证据 / Evidence** — Sprint 44 CHANGELOG 条目; `cargo test
  --workspace --locked` 全绿 (新增 8 单测); clippy `-D warnings` 0;
  H1-H8 + I1-I8 + N1-N5 三套 e2e 全绿。
- **卡点 / Blockers** — none
- **依赖 / Depends on** — T4.13

---

## T4.15 — 内置引擎 host-direct 字面量 `/` 执行 (Q60)

- **目标 / Goal**
  在 T4.11–T4.14 已完成的基础上新增 **tenant-dedicated 集群内普通推理的
  host-direct 性能画像**，不重开旧卡。k3as 是多租户 Agent sandbox 平台，
  支持的部署模型是一租户一集群且节点专属该租户，Kubernetes Namespace 不是
  租户安全边界。仅当节点显式选择 `--container-engine=builtin`、
  `--builtin-rootfs=host`，且 Pod 带 `init-pro.io/rootfs: /` 时，普通推理容器
  直接使用字面量宿主 `/`。它消除 Q55 的字面量 `/` 限制，但不改变 Q51 的全局
  `containerd` 默认值；推理垂直切片必须显式选择 builtin。

- **核心实现 / Core implementation**
  - rootfs 解析在 **kubelet sync image planning 之前**完成严格三重选择；只有
    builtin + `--builtin-rootfs=host` + 注解值恰好 `/` 进入 host-direct。此路径的
    `image` 仅为 retained declarative/wire metadata，literal `/` 而非 image 选择
    host rootfs。create/restart/reconcile 对 workload 与 pause/sandbox 均不得发出
    `Action::EnsureImage`，也不得 pull、presence check、ref/CAS/layer resolve 或
    materialize、merge image config、copy/merge layers，或对 `/` 执行 `pivot_root`。
    omitted/defaulted 及显式合法的 Always/IfNotPresent/Never 均接受但行为无关；
    显式非法 policy 以 Kubernetes-style validation 拒绝，不得静默默认化。
    Q55 普通 host-directory presence-only 行为保持不变；其他错误组合返回可操作
    Pod event/status，不猜测用户意图。
  - `clone3` 继续使用 NEWNS、NEWPID、NEWUTS、NEWIPC、NEWCGROUP，且不加
    NEWNET（Q52）。literal `/` 的 child setup 先做安全 mount propagation 隔离，
    再在私有挂载命名空间处理新 PID namespace 所需的 `/proc`；不执行 `pivot_root`。
  - 不换成裁剪的 tmpfs `/dev`：保留宿主 `/dev` 可见性，供 tenant-dedicated
    节点上的普通 accelerator 推理直接访问设备。T4.14
    `init-pro.io/host-mounts` 的 `:ro`/`:rw` 语义与 T4.9 CPU/memory cgroup
    限额继续生效。
  - host-direct 或 GPU 请求不得回退 containerd（无论是否静默或显式）。Q51 的
    普通 builtin boot 探针回退保持不变；若节点因此不能提供 host-direct/GPU，请求
    必须显式失败或保持 Pending 并给出原因，绝不在另一引擎伪成功。
  - **此画像不是 Agent action containment，也不是跨租户边界。** 一租户一集群、
    节点不跨租户共享是 hostile multi-tenant support 的必要但非充分条件；还须满足
    Q60/Phase 3 §7 所列并实测的 per-tenant 网络、存储、端点及凭证隔离，否则该
    部署不受支持。宿主 `/`、`/dev` 和 host netns 仅用于普通推理。Agent 必须使用
    Q63 的私有非宿主 rootfs、fresh network namespace 与 action gate。

- **验收手段 / Acceptance**
  - Q59 `phase3-v1` golden 与单测覆盖严格三重选择正例、缺项/错误组合及非字面量
    `/` 负例、host-direct/GPU 禁止 containerd fallback；逐一验证 omitted/default
    与 Always/IfNotPresent/Never 在 create/restart/reconcile 对 workload/pause 均
    零 `Action::EnsureImage` 和零 image processing，显式非法 policy 被拒绝。
    同时证明 Q51 默认引擎、Q55 host-directory presence-only 与 Q63 配置边界不变。
  - 特权 Linux e2e 验证宿主标记从 `/` 可见，以 runtime traces 证明无 image
    action/pull/presence/ref/CAS/layer/config/copy/merge/pivot；namespace 集合无
    NEWNET，`/proc` 只呈现新 PID namespace，并可见一个预置伪设备节点；无需真实 GPU。
  - 回归 host-mounts ro/rw、CPU/memory limits、logs/exec、H1–H8/I1–I8/N1–N5；
    强制启动失败后断言没有 containerd 容器被创建。T0.6 golden 与常用质量门
    必须全绿。

- **状态 / Status** — in-progress (S47: 实现全量落地——`kubelet/src/engine/host_direct{,_run}.rs` 严格三重选择 + nsrun/engine host-root 子配置 (NEWNS/NEWPID/NEWUTS/NEWIPC/NEWCGROUP, 无 NEWNET, 无 pivot_root, 宿主 /dev 与 /sys 直通, 新 PID ns 的新鲜 /proc, private propagation 先行), workload+pause 均无 EnsureImage, HD5 破坏三重拒绝; fake/本机 root 证据: `builtin-hostroot-e2e.sh` HD1-HD5 + H1-H8, `engine-selftest.sh` 33/33 (E12/E12b host-root), `builtin-runtime-e2e.sh` B1-B13+N1-N5; 剩余: node04 真机 HD 面)
- **证据 / Evidence** — S47 gates (fake/offline + 本机 root): builtin-hostroot H1-H8+HD1-HD5 PASS, engine-selftest 33/33, builtin-runtime B/N PASS, golden G90-G96 (含 `/`+Recreate 正例), workspace tests 611 passed。真硬件/hostile 场景与 node04 现场 HD 验收未做——card 未关闭。
- **卡点 / Blockers** — 需要特权 Linux 测试宿主；真实 accelerator 验证留到
  T7.2/T7.6。operator 必须保证集群与节点归属单一租户，且不得把该拓扑当作完整
  hostile multi-tenant 边界；宿主 `/`、`/dev` 和 host netns 暴露只用于普通推理，
  绝不作为 Agent 或跨租户隔离边界。
- **依赖 / Depends on** — T4.14
