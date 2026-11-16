# Layer 4 — Node / Agent

Mirrors `index.md` TODO IDs **T4.1–T4.6**. The node side: bundled
containerd + CRI, a kubelet equivalent, CNI networking, network policy,
and node registration.

---

## T4.1 — containerd bundling 与 CRI 实现

- **目标 / Goal**
  Bundled containerd (single binary, Q1) exposing a CRI socket the kubelet
  equivalent talks to (k3s `pkg/agent/containerd/` parity).

- **核心实现 / Core implementation**
  - Embed containerd via T0.2; configure
    (`link_repos/k3s/pkg/agent/containerd/config_linux.go` parity).
  - Supervise as `init-pro containerd` (multicall alias, T0.1).
  - CRI plugin enabled; init-pro kubelet uses the CRI gRPC API.
  - Default runtime class + image pull policy defaults.

  - **Spike (Sprint 15, Q24 — superseded by the as-built below):**
    `scripts/t41-containerd-spike.sh` (5/5) proved the chain — vendored
    containerd 1.7.20 + runc 1.1.13 + cni-plugins 1.5.1 staged k3s-style
    and booted through the multicall seam, CRI plugin `ok`.

  - **As-built (Sprint 16, Q25/Q26):** new `crates/runtime`:
    `config.rs` renders TOML v2 from `ContainerdConfigVars::for_data_dir`
    (CRI plugin enabled; `sandbox_image` from `INIT_PRO_SANDBOX_IMAGE`,
    default `registry.k8s.io/pause:3.10`); `stage.rs` does idempotent
    SHA-256 staging of the containerd tree (containerd, ctr, shims,
    runc, crictl, `aux/` cni-plugins) plus the CNI loopback conflist
    `10-init-pro.conflist`, with `vendor_bin_root` resolving
    `INIT_PRO_VENDOR_BIN` -> exe-relative `../../vendor/bin` -> cwd;
    `supervisor.rs` gates boot on socket health (UnixStream poll), backs
    off `base << restarts` capped at 5 s, resets the ladder after
    STABLE_AFTER 30 s, and drains via SIGKILL + bounded 10 s wait —
    deliberately **no SIGTERM step** (containerd child-reaping is not
    guaranteed for a foreign runtime; k3s kills the tree too — Q25).
    `infra::signal::Shutdown` is now **sticky** (fired flag +
    `Notified::enable`), fixing lost wakeups during select gaps for
    every consumer. CLI agent wiring calls `start_agent_runtime` and
    drains the runtime FIRST on shutdown; `init-pro crictl ...` /
    `init-pro ctr ...` are intercepted pre-clap and re-exec the staged
    peer with endpoint injection (`crictl_endpoint_args` /
    `ctr_address_args` in multicall). `crictl` v1.31.1 is pinned in
    `vendor/versions.toml` (sha256
    `0a03ba6b1e4c253d63627f8d210b2ea07675a8712587e697657b236d06d7d231`,
    staged like every peer).
    CRI client strategy per **Q26**: route B (crictl subprocess) now,
    route A (native gRPC, ~100-crate dep cost) only when T4.2 needs
    streaming/watch.

- **验收手段 / Acceptance**
  - `init-pro crictl ps` / `init-pro ctr` round-trips; a sandbox container runs.
  - As-built: runtime 17 unit + 2 integration tests
    (`crates/runtime/tests/supervisor_integration.rs`: kill -9 rebirth +
    crictl round-trip over the live CRI socket; SKIP, not fail, when the
    vendor bin is absent), multicall 15, golden **G24**
    (`scripts/golden-conformance.sh` — agent supervises containerd,
    `crictl version`/`ps` over CRI; sandbox-pull smoke SKIPs when the
    registry is unreachable).

- **状态 / Status** — done
- **证据 / Evidence** — runtime 17+2 / multicall 15 / infra 29 tests;
  golden G24; Q25/Q26 ADRs
- **卡点 / Blockers** — none (debt for T4.2: image pre-pull/airgap
  store, cgroup delegation, native CRI client trigger per Q26)
- **后续项 / Follow-up — FIXED (Sprint 28, Q24-Q26 域)** — supervisor 关停
  顺序竞态（Sprint 27 取证：慢的 pod teardown 被孤儿化 shim，宿主残留
  shm/overlay 挂载）按既定方向修复：**先停 task/sandbox → 等 CRI 清零 →
  再停 containerd**。两层根因均修：(1) 装配层——supervisor 只听共享
  Shutdown token，join 顺序只排 await 不排实际停机；改为专用
  `runtime_shutdown` token，cli drain 顺序 kubelet join → CRI sandbox 清零
  确认（`wait_sandboxes_gone` 5s 有界轮询，与 harness 宽限窗对齐）→
  trigger supervisor → flannel → anyhome（`crates/cli/src/runtime.rs`）。
  (2) teardown 形状——kubelet 收到 shutdown 原本直接 return；新增
  `sync::drain_sandboxes`（15s 预算内逐 sweep：每个 sandbox **先 stop/rm
  其容器 task，再 stopp/rmp**；NotFound 容忍、瞬态拒绝 250ms 退避下个
  sweep 重试）。task-first 是承重墙：对带 RUNNING 容器的 sandbox 直接
  StopPodSandbox 会在请求内停活容器、撞破 crictl 2s 截止
  （DeadlineExceeded），rmp 随即 "is running" 弹回；同步路径因 `plan`
  分周期 teardown 从未踩到。harness `purge_run_shims` 退化为
  `assert_no_run_shims`（残留 = FAIL；杀/umount 仅为测试主机卫生，四个
  per-suite 调用点同批转换）。验收证据（当次复跑）：压负载（nice 19 双
  忙循环）3× golden **39/0 全绿、exit 0、`zero-shim-leftovers` 零触发、
  无需 purge 兜底**；微 e2e 实测 drain 时序 `teardown done(sandboxes=1)
  → CRI drained(13ms) → containerd drained`，随后 datadir shims=0
  mounts=0（对照：一次性 stopp/rmp 下 shims=1 mounts=2）。Tests: 935 →
  940（NEW `tests/sync_drain.rs` 5 例，含容器先于 sandbox 的顺序回归钉）。
- **依赖 / Depends on** — T0.2

---

## T4.2 — kubelet 等价 (pod 生命周期 + status)

- **目标 / Goal**
  A Rust kubelet equivalent: pod sync loop, status reporting, volume mount,
  probes, eviction — API-compatible with what the API server expects.

- **核心实现 / Core implementation**
  - `init-pro-kubelet`: list/watch pods for its node, reconcile desired→actual
    via CRI (T4.1) + volume plugins.
  - Status上报 via `/api/v1/nodes/<name>/status` + pod status subresource.
  - Readiness/liveness/startup probes; graceful pod termination.
  - Node condition + capacity/allocatable reporting.

  - **As-built (Sprint 17, Scope A):** new `crates/kubelet` (15th
    workspace crate, pure-functional; 11 src files + 3 test files):
    `kubelet::spawn(cfg: KubeletConfig, cri: Arc<dyn CriBackend>,
    shutdown: infra::Shutdown) -> Vec<JoinHandle<()>>`,
    `KubeletConfig::new(server_url, node_name, data_dir)` (+
    `sandbox_image` from `INIT_PRO_SANDBOX_IMAGE`), and
    `kubelet::default_node_name()`. Minimal HTTP/1.1 client (`http.rs`
    + `framing.rs` chunked decoder) and watch stream (`watch.rs`):
    LIST+watch `/api/v1/pods` keeping a desired map filtered by
    `spec.nodeName == node` — own HTTP stack because the crate must
    stay dependency-light and the apiserver is HTTP-only (Q21).
    Level-driven sync loop (`sync.rs`/`exec.rs`) diffing desired vs
    CRI snapshot → sandbox/container actions; status builder
    (`status.rs`: phase Running/Pending, PodScheduled+Ready
    conditions, containerStatuses with `cri://` ids,
    restartCount=attempt); node registration + kube-node-lease Lease
    heartbeat (`node.rs`); status writes via `PUT pods/status`.
    CRI via the vendored crictl (**Q26** route B): `crates/runtime`
    gains `cri.rs` + `cri_json.rs` — a `CriCtl` driver implementing
    12 CRI ops (runp/stopp/rmp/create/start/stop/rm/ps/pods/images/
    import/pull/version-class) over the staged binary, plus a serde
    layer over `crictl -o json`; v1.31.1 quirks codified (`stop`/`rm`
    verb names, explicit `stopp` before `rmp`, default 2 s RPC
    timeout flake → explicit `--timeout` on stop).
    apiserver side: `PUT /api/v1/namespaces/<ns>/pods/<name>/status`
    subresource (`crates/apiserver/src/pod_status.rs`: read-first,
    merge only `.status`, CAS; 404/422/409 semantics). Agent wiring:
    `--node-name` flag; kubelet spawned from the agent branch when
    `--server` is `http://` (https:// URLs rejected with a warning
    and the kubelet skipped — keeps golden G24 green); subcommand-
    aware flag-strip filter; drain order kubelet → runtime →
    apiserver → controllers → scheduler.
    Workload image per **Q27**: `scripts/build-pause-image.sh` builds
    a static pause binary (gcc `-static -Os`) into a hand-assembled
    OCI layout — ref `init-pro.local/pause:0.1`, imported via the
    staged `ctr` into namespace k8s.io (`INIT_PRO_DATA_DIR=<dd>
    init-pro ctr -n k8s.io images import <tar>`), agent started with
    `INIT_PRO_SANDBOX_IMAGE=init-pro.local/pause:0.1`. CNI
    requirement: containerd CRI hard-fails `runp` unless the sandbox
    gets an eth0 IP → `crates/runtime/src/stage.rs` conflist changed
    loopback-only → bridge + host-local (10.42.0.0/24 subnet).
    Two bugs fixed (both exposed by G25): `pods/status` +
    `pods/binding` CASed on the stored blob's embedded
    `metadata.resourceVersion`, which lags `mod_revision` after any
    client read-modify-write (deterministic 409 loop) — now CAS on
    `entry.mod_revision`; apiserver graceful drain waited forever on
    open watch streams (the kubelet holds one) — now a 2 s
    `DRAIN_GRACE` deadline in `serve.rs`.
    Sprint 18 (S1) extension: the kubelet reports podIP/podIPs +
    hostIP=127.0.0.1 — `crates/runtime/src/cri_json.rs` parses
    `crictl inspectp` output (`PodSandboxInspect` + `cri.rs`
    `inspect_pod_sandbox`) and `crates/kubelet/src/status.rs`
    surfaces `status.network.ip`; the node object gained an
    InternalIP address (`crates/kubelet/src/objects.rs`).

- **验收手段 / Acceptance**
  - Golden (T0.6): run a Deployment pod to `Running`+`Ready`; kill the
    container, observe restart; delete pod, observe terminated.
  - As-built (Scope A): golden **G25** — exactly those three
    assertions (pod Running+Ready on the agent node; killed container
    restarted with a new id; deployment delete tears down to zero
    sandboxes), gated on vendor bundle + `cc`, SKIP-not-fail like
    G24. Tests: 551 → 631 workspace (kubelet 53; apiserver 48 → 51
    incl. +5 pods/status wire tests; +17 runtime CRI driver incl.
    live-containerd integration; new `tests/drain_deadline.rs`).

  - **As-built (Sprint 20, Scope B):** probes + volumes + pull policy
    + hostname landed (S1–S5): 探针引擎 `crates/kubelet/src/probes.rs`
    (399 行, HTTP-only `ProbeSpec`/`Tracker` 状态机, startup 门控 +
    readiness/liveness + liveness 失败→重启容器 + restartPolicy 门控
    + Ready 条件派生 + `ContainerView.exitCode` 透出)；卷
    `crates/kubelet/src/volumes.rs` (400 行, emptyDir 按 volume name +
    configMap/secret 投影按 object name + FNV-1a 内容指纹原地刷新 +
    sandbox 清理联动; `Mount` struct + `ContainerConfig.mounts`
    (Box 修复 large_enum_variant))；imagePullPolicy 默认化
    (`ContainerSpec.image_pull_policy` 解析时默认: latest/无 tag→
    Always, digest→IfNotPresent, 显式 Never; `ensure_image` 带 policy
    参数, sandbox 镜像恒 IfNotPresent)；`PodSandboxConfig.hostname`
    透传 = pod name (skip-if-empty, S4)。golden **G26a–d** 端到端
    (探针门控 Running+Ready、/fail → liveness 重启 restartCount≥1 →
    /healthz 恢复 + Ready、crictl stop → emptyDir 跨重启 bootcount==3、
    configMap 投影 + ECHO==pod name；echo 镜像确定性构建新增
    /healthz+/fail+/bootcount+CONFIG 行)。门禁 31/31 ×2 无 SKIP +
    ST1–ST6 6/6 + D1–D4 4/4 + clippy 0。

  - **As-built (Sprint 24, Scope C):** logs/exec 数据面走上游形状的三段
    链路 (**Q31**): (1) apiserver 中继 `crates/apiserver/src/pod_io.rs`
    (280 行) + `kubelet_hop.rs` (253 行)，路由
    `/api/v1/namespaces/{ns}/pods/{name}/log|exec`，寻址链
    Pod→nodeName→Node InternalIP + 注解 `init-pro.io/kubelet-port`
    (回退 10250)，hop 失败 → 404/502；(2) kubelet 局部监听器
    `crates/kubelet/src/server.rs` (364 行, axum 升正依赖) +
    graceful drain；(3) crictl 子进程 `runtime::CriCtl::exec_container`
    (`--timeout` 裸秒数修复, 非 0 rc 不当错误)。CLI 新增
    `--node-ip/--kubelet-port`（含 nop-strip 豁免）；kubectl 新动词
    `logs [-f|--tail] [-c]` / `exec POD -- CMD…`，`INIT_PRO_SERVER`
    env 回退全动词生效；`-f` 为客户端 250ms 轮询（记录在案的偏差）。
    缺陷修复：metadata.uid 此前无人分配 —— apiserver create、SSA 创建
    分支、controllers StorageClient.create 三处写入接缝统一注入。
    golden **G30** 四断言 → **38/38 全绿** (vendor+cc 门控; 随机端口
    证明注解寻址)。local-up.sh 单机 UX：健康后导镜像、REST 一键 demo.sh
    (kubectl apply/get 现仅 argoproj.io v1alpha1)、LOCAL_UP_DATA_DIR
    持久化模式。测试 workspace 903/0；clippy 0; parity 16/16;
    discovery ALL。

  - **As-built (Sprint 27, `nfs` 卷类型 — T-anyhome, Q36):** kubelet
    volumes 新增第四种 inline 卷 `nfs`（k8s `NFSVolumeSource` 形状:
    `server`/`path` 必填, `readOnly` 缺省 false —— `objects.rs::nfs_source`,
    server/path 缺失或为空的条目跳过不报错）。挂载走宿主机 `mount.nfs`
    子进程（`nfs_mount.rs`, 经 PATH 查找）：
    `mount.nfs -o vers=3,tcp,nolock,noacl[,port=P,mountport=P]
    server:path target` —— anyhome 把 NFS/MOUNT/portmap 复用在 ONE
    端口上，所以 `port=` 与 `mountport=` 是同一个值（上游 README 的
    官方挂载行）；`server` 无可用数字端口时省略 `port=` 选项，内核
    缺省 2049 生效。幂等性来自 `/proc/self/mounts` 内省（内核八进制
    转义解码）：已是挂载点则跳过；清理 `cleanup_volumes_in` 先
    `umount`（嵌套挂载按路径深度倒序、子先于父）后删目录 —— umount
    失败保留整棵树并返回 Err，绝不穿过活挂载点删除内容。测试策略：
    纯 helper 与子进程 runner 分离，测试注入 fake 可执行脚本（fake
    `mount.nfs`/`umount` 记录 argv、可注入退出码），不污染进程环境。

- **状态 / Status** — done (Scope A+B+C, Sprint 24; **Q31**; Sprint 27
  补 `nfs` 卷类型/Q36)
- **证据 / Evidence** — Sprint 24: workspace 903 通过 / 0 失败;
  golden **38/38** ×(G25/G26/G30 vendor+cc); flag parity 16/16;
  discovery parity ALL; clippy -D warnings 0. 前序: golden G25 27/27,
  G26 31/31 ×2, ST1–ST6 6/6, D1–D4 4/4 (Sprint 17/20 evidence 保留)。
- **卡点 / Blockers** — 无 (A+B+C 全部交付)。**S37c 实机跟进
  (候选工单)**: F3 — agent 重启后 ~12 s 内 logs/exec 中继 500 (embedded
  containerd socket 未就绪; 验收脚本已用轮询吸收, 建议 relay 侧短重试
  或就绪门)。见 `验收报告.md` F3。
- **顺延 / Deferred (future enhancements, 非 TODO 阻塞)** — probes
  TCP/exec 类型；imagePullSecrets（私有仓库认证）；CSI 卷；
  exec attach/port-forward 流式子协议；kubelet 监听器 TLS + authN
  （随 T1.3 安全工作面）。
- **依赖 / Depends on** — T4.1, T3.2
- **As-built (Sprint 37c, 2026-11-15, 双节点实机)** — heartbeat 的
  GET-modify-PUT 顺带 reconcile Node 注解 `init-pro.io/kubelet-port`
  (纯函数 `reconciled_kubelet_port_annotation`, 已正确则跳写; commit
  `1242c5f`, 3 单测)。修复前: 改 `--kubelet-port` 重启后 relay 拨旧端口。
  实测: 10250→10260 注解 6 s 收敛、中继跟随, 回退同理。验收沉淀:
  `scripts/acceptance-2node-e2e.sh` (A14) + `scripts/build-netpod-image.sh`;
  遗留发现见仓库根 `验收报告.md` (同名 Agent 重建不 re-reconcile = F4、
  agent 重启 ~12 s 中继竞态 = F3、sandbox 不收养 = F2)。

---

## T4.3 — CNI/网络 (flannel 等价 + ServiceLB L4)

- **目标 / Goal**
  Pod networking via a bundled CNI (flannel-equivalent VXLAN/Host-GW) +
  cluster Service VIP handling + L4 load balancing (ServiceLB, k3s).

- **核心实现 / Core implementation**
  - CNI plugins bundled (T0.2); node-pod CIDR allocation via API
    (`node.spec.podCIDR`).
  - Flannel-equivalent overlay in Rust or supervised flannel binary;
    configure via `link_repos/k3s/pkg/agent/flannel/setup.go` parity.
  - kube-proxy-equivalent iptables/nftables (or eBPF) for Service ClusterIP.
  - ServiceLB: DSR/local L4 LB for `LoadBalancer` Services on bare metal
    (k3s `servicelb`).

    Note (Sprint 18, **Q28**): the Service NodePort plane does NOT
    live here — it is carried by the built-in Router (`crates/router`:
    one reverse-proxy listener per allocated nodePort, on by default,
    `--disable-kube-proxy` opts out). T4.3 still owns the pod-network
    CNI and any future ClusterIP dataplane.

  - **Scope A as-built (Sprint 25, Q34 — CNI 换底 flannel-rust):**
    - 内嵌 flanneld：`crates/cli/src/flannel.rs` 把 flanneld daemon
      （flannel-rust 上游, 默认 vxlan backend）跑在 agent 进程内；
      上游 `install_signal_handlers` 关闭（wave 0 上游改造），SIGTERM 经
      init-pro 自身 infra::Shutdown 的 CancellationToken 桥接。
    - CNI meta-plugin：multicall 别名 `flannel` →
      `flannel_cni::skel::run()`，由 containerd 经
      `<runtime>/aux/flannel` symlink 调用（runtime::stage 负责 aux
      staging + symlink）。
    - node podCIDR 分配：`crates/controllers` 新 node-cidr reconciler
      （`Kind::NodeCidr`），按 `ManagerConfig.cluster_cidr`
      （`--cluster-cidr`, 默认 10.42.0.0/16）给每个 Node 写 `/24` podCIDR。
    - conflist + 修复：`runtime::stage` 新增 `10-flannel.conflist`；
      C 侧 delegate 去掉 `isDefaultGateway`（双默认路由）、钉住
      `iptables_forward_rules: true`（FORWARD-DROP 杀 pod↔pod）。
    - Node 条件：`crates/cli/src/flannel_keeper.rs` 以 5s 循环维持
      `NetworkUnavailable=False`。**债务**：apiserver status patch 缺
      `status/conditions` merge key，一次性条件写入尚不可行
      （upstream 式 status subresource merge 待做）。
    - 新 CLI flag：`--flannel-backend`、`--cluster-cidr`。
  - **As-built (Sprint 35, Q41)：** ServiceLB L4 / LoadBalancer VIP
    announce 已落地 —— `controllers/service_lb.rs` announce 控制器
    （klipper-lb 语义：显式 `spec.loadBalancerIP` 优先，否则首 Node
    InternalIP；非空 ingress 永不改写；CAS status 写 + 限速重排；
    `--disable servicelb` 关闭）+ `router/servicelb.rs` raw-TCP L4 面
    （`vip:servicePort` 监听 round-robin ready Endpoints；bind 失败 =
    announce-only 单条 warn；空后端连接立即 FIN；NodePort 面同时放行
    LoadBalancer 型 Service）。
  - **剩余 T4.3 范围（not-started）：** 上游 flannel conformance 面
    （host-gw / wireguard backend、multi-node vxlan、netpol）。
  - **Scope C (S42 排期, Q44 草案)：** 分配面 —— `--service-cidr` 从
    noop 表转 wired，`service.rs` noop 分支改为从 CIDR 池分配 ClusterIP +
    冲突 CAS 自愈（复用 NodePort 现有分配模式）；承载面 —— 仿
    `servicelb.rs` 新 listener 类型（bind clusterIP:port，RR 就绪
    Endpoints，`copy_bidirectional`；bind 依赖 IP_FREEBIND 或本机回环
    别名，取舍由 Q44 定夺）；headless（`clusterIP: None`）不分 IP 不监听
    （为 T6.2a DNS 留数据源）。明确 out: sessionAffinity/UDP/
    externalTrafficPolicy（Q28 原文保留）。验收: 分配/冲突单测 +
    service-traffic-e2e ST7–ST10。

- **验收手段 / Acceptance**
  - Golden: pod-to-pod across nodes; ClusterIP reachable; LoadBalancer
    Service gets an external IP and routes.
  - As-built (Scope A): `scripts/flannel-e2e.sh` F1–F5 on a real
    single-node cluster — F1 node podCIDR 从 cluster-cidr 分配；F2 flannel
    注解（`flannel.alpha.coreos.com/*`）+ `NetworkUnavailable=False`；
    F3 podIP 落在 node podCIDR 内；F4 flannel 织物上 pod↔pod 可达；
    F5 信息性说明（上游 conformance host-gw/multi-node/netpol 不在
    Scope A 内）。

- **状态 / Status** — in-progress (Scope A flannel CNI done, Sprint 25, Q34;
  Scope B ServiceLB announce/L4 done, Sprint 35, Q41)
- **证据 / Evidence** — Sprint 25: flannel-e2e F1–F5 5/5（F5 信息性）；
  service-traffic ST1–ST6 6/6（NodePort 面, Q28/Q32, 不属本卡）；
  workspace 852 tests green; flannel multicall selftest 9 别名含 flannel。
- **卡点 / Blockers** — 债务两项：flannel_keeper 依赖 5s 轮询（缺
  `status/conditions` merge key，upstream 式 status subresource merge
  待做）；`ingress_listen` CLI flag 未接线（router 侧, T5.5/T5.7 相邻）。
  LoadBalancer VIP 需要 T5.6 dataplane 协调。
- **依赖 / Depends on** — T4.2

---

## T4.4 — 网络策略 (netpol 等价)

- **目标 / Goal**
  `NetworkPolicy` enforcement (k3s `pkg/agent/netpol/` parity): allow/deny
  ingress/egress by label/namespace selectors.

- **核心实现 / Core implementation**
  - Translates NetworkPolicy objects into dataplane rules consistent with
    T4.3's chosen dataplane.
  - Watches policies + pods/endpoints; reconciles rule sets.

- **验收手段 / Acceptance**
  - Golden: a `default-deny` namespace blocks traffic; a selected pod is
    allowed; egress policy honored.

- **状态 / Status** — not-started
- **证据 / Evidence** — —
- **卡点 / Blockers** — none
- **依赖 / Depends on** — T4.3

---

## T4.5 — 节点注册/心跳/代理隧道

- **目标 / Goal**
  Node registration with the API server, heartbeat (lease/Status), and the
  server↔agent tunnel (k3s `pkg/agent/tunnel`, `pkg/daemons/control/tunnel.go`).

- **核心实现 / Core implementation**
  - Node object bootstrap on agent start; `NodeLease` heartbeat loop.
  - Tunnel: agent dials server; multiplexed reverse proxy for apiserver
    egress from server-side controllers (k3s `--node-external-ip`/proxy).
  - Reconnect/backoff; credentials from T3.3.

- **验收手段 / Acceptance**
  - Golden: agent joins, `kubectl get nodes` shows `Ready`; sever tunnel,
    observe reconnect within SLA.

- **备注 / Note (Sprint 17, T4.2 Scope A as-built):** node registration,
  `kube-node-lease` heartbeat, and `kubectl get nodes` Ready are already
  delivered by T4.2 Scope A (`crates/kubelet/src/node.rs`). T4.5's unique
  remaining scope = the server↔agent tunnel, reconnect SLA, and
  credentials (T3.3).
- **状态 / Status** — not-started
- **证据 / Evidence** — —
- **卡点 / Blockers** — none
- **依赖 / Depends on** — T4.2, T1.2

---

## T4.6 — Pod 日志布局与轮转 (日期目录 + 100MB copytruncate)

- **目标 / Goal**
  Container log files carry a per-day directory segment and never exceed
  100 MiB on disk per live file: `<data_dir>/pods/<pod-uid>/<YYYY-MM-DD>/
  <container>.<attempt>.log`, rotated externally (copytruncate) with N=5
  retained generations. This is the k8s-kubelet `containerLogManager`
  shape, sanctioned by Q31's already-accepted external-rotation deviation
  and locked as **Q39**.

- **核心实现 / Core implementation**
  - Write side (`kubelet/src/sync.rs`): `plan()` takes an INJECTED UTC
    date (stays pure) and composes the sandbox `log_directory`
    `<log_root>/<uid>/<date>/` (`log_rotate::dated_dir`); the container
    `log_path` stays RELATIVE (`<name>.<attempt>.log`) so containerd
    joins the pair itself and opens/holds the fd once. The date is pinned
    at plan time — a container alive across midnight keeps writing into
    its own (older) date dir. `exec.rs` mkdirs the dated dir before
    `runp` (unchanged mechanism).
  - Date source: `common::time::utc_date` (Hinnant civil date, same
    algorithm family as `now_rfc3339` — no chrono).
  - Rotator (`kubelet/src/log_rotate.rs`): pure `rotation_plan` +
    `rotate_one` copytruncate (copy -> `.1`, shift `.N-1` -> `.N`,
    evict `.N`, truncate the live file in place — writer fds stay
    valid) + `sweep` over ONLY date-shaped dirs under each pod dir (the
    `volumes/` sibling can never match). Constants: 100 MiB / 5
    generations / 60s scan (`LOG_MAX_SIZE`/`LOG_MAX_FILES`/
    `SCAN_PERIOD`, `LogRotatePolicy::default()`).
  - Runner (`runner.rs`): 5th task `log_rotate_task` — periodic
    `spawn_blocking` sweep; per-file failures are logged and retried on
    the next tick.
  - Read side (`server.rs::resolve_log_path`): highest attempt across
    EVERY date dir (a still-running pre-rollover container lives in an
    older dir while a sibling's restart made a newer one), newest date
    as tie-break; pre-T4.6 flat layout kept as a fallback so
    upgraded-node legacy containers stay readable. `kubectl logs -f`
    already handles file-shrink (truncate) on the client side.

- **验收手段 / Acceptance**
  - Unit: `log_rotate.rs` (date shape, plan shifts/eviction, copytruncate
    semantics, sweep scoping to date dirs + volumes/legacy immunity,
    retention cap, missing-root no-op), `server_tests.rs` (cross-date
    resolution + legacy fallback + route-L over HTTP on the dated
    layout), `sync_plan.rs` (dated `log_directory` + relative
    `log_path` pinned), `common::time::utc_date`.
  - Integration: `kubelet_http_integration` (5 loops incl. the rotation
    task; full spawn against FakeCri stays green).
  - e2e (>100MB in-pod churn driving an actual split): optional follow-up
    under `scripts/` — not gating.

- **状态 / Status** — done (Sprint 32)
- **证据 / Evidence** — `crates/kubelet/src/log_rotate.rs`,
  `crates/kubelet/src/{sync,server,runner}.rs`,
  `crates/common/src/time.rs`; `cargo test -p kubelet -p common` green.
- **卡点 / Blockers** — none
- **依赖 / Depends on** — T4.2

---

## T4.7 — 容器 env/envFrom 投射

- **目标 / Goal**
  Pod spec 的 env (literal + fieldRef-free) 与 envFrom (ConfigMap/Secret)
  在容器创建时投射进 CRI `envs`。

- **核心实现 / Core implementation**
  - `crates/kubelet/src/objects.rs` ContainerSpec 增 `env`/`env_from`
    解析 (literal + ConfigMap/Secret ref)；`optional` 语义: 缺 required
    ref → 容器创建失败事件。
  - `sync.rs` 的 `envs: Vec::new()` 改为 create 时解析填充（对齐 k8s:
    env 不热更新，无 watch 联动）。
  - CRI 层 `cri_json.rs` envs 字段已存在，零改动。
  - 明确 out: Downward API、projected env。

- **验收手段 / Acceptance**
  - 解析/解析失败单测 + 新 `scripts/pod-env-e2e.sh` E1–E4（容器
    printenv 经 logs 断言；vendor bundle 缺席则 SKIP，Q25 教义）。

- **状态 / Status** — done (Sprint 38)
- **证据 / Evidence** — `crates/kubelet/src/env.rs` (357) + `env_fetch.rs`
  (212) + `env_tests.rs` (274, 10 例) + `tests/sync_plan.rs` (+2); e2e
  `scripts/pod-env-e2e.sh` E1-E4b 5/5 绿 (printenv 经 log subresource 断言,
  CRI 帧剥离); workspace **998 passed / 0 failed**; clippy 0; parity 20/20。
- **卡点 / Blockers** — none
- **依赖 / Depends on** — T4.2

---

## T4.8 — 探针补全 (tcpSocket + exec)

- **目标 / Goal**
  httpGet 之外补 tcpSocket/exec 两类探针。

- **核心实现 / Core implementation**
  - `probes.rs` 扩 tcpSocket（tokio connect，数字端口）与 exec
    （复用 `CriCtl::exec_container`，rc==0 即健康，无 shell 语义照抄
    k8s）；`runner.rs` 分派。
  - 明确 out: 命名端口（IntOrString 顺延，Deferred 注记）。

- **验收手段 / Acceptance**
  - 本地 listener 单测 + e2e 探针成功/失败双路径（失败路径驱动重启
    可见）。

- **状态 / Status** — not-started (S39 排期)
- **证据 / Evidence** — —
- **卡点 / Blockers** — none
- **依赖 / Depends on** — T4.2

---

## T4.9 — 资源限制 → CRI linux.resources + QoS 分级

- **目标 / Goal**
  Pod resources.limits/requests 运行时强制进容器 cgroup。

- **核心实现 / Core implementation**
  - `cri_json.rs` 新增 `linux.resources` 结构（沿用 crictl proto
    字段名惯例）。
  - `objects.rs` 解析 `resources.limits/requests`（调度侧已消费
    requests，本卡补运行时强制）。
  - `sync.rs` 映射 cpu shares/quota/period + memory limit；新增纯函数
    QoS 分级（Guaranteed/Burstable/BestEffort）。
  - 明确 out: 节点真实 capacity 探测、eviction。

- **验收手段 / Acceptance**
  - 映射/QoS 单测 + e2e（容器内 `cat /sys/fs/cgroup/memory.max`
    断言）。

- **状态 / Status** — not-started (S40 排期)
- **证据 / Evidence** — —
- **卡点 / Blockers** — none
- **依赖 / Depends on** — T4.2

---

## T4.10 — kubelet 卷消费扩展 (hostPath + PVC)

- **目标 / Goal**
  kubelet 能消费 HostPath 与 PVC 卷（经 bound PV 解析真实路径）。

- **核心实现 / Core implementation**
  - `objects.rs` VolumeKind 增 `HostPath` + `Pvc`。
  - `sync.rs` mounts 组装（PVC → bound PV → hostPath 路径解析）。

- **验收手段 / Acceptance**
  - `scripts/storage-e2e.sh` V4–V6 —— STS 的 PVC 真实 Bound 并可写，
    pod 重建后数据仍在（Q22 的 "Pending forever" 就此 Superseded，
    Q46）。

- **状态 / Status** — not-started (S45 排期)
- **证据 / Evidence** — —
- **卡点 / Blockers** — 依赖 T6.2b provisioner 先落地 (S44)
- **依赖 / Depends on** — T4.2, T6.2
