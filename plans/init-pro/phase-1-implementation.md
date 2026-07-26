# Phase 1 实施计划（M0 基座 + M1 Router spike）

> Status of this file: **active**. Tracks the Phase 1 execution plan derived
> from `index.md` / `README.md`. Sprint outcomes are recorded inline as they
> land. TODO IDs strictly mirror `index.md`.

## Goal（目标）
交付 **Phase 1 = M0 基座 + M1 路由器 spike**：先证明两条最高风险赌注 ——
**单一二进制 multicall（Q1）** 与 **内置 Lua Router（Q4）** —— 再投入完整平台建设。
Phase 1 结束即可演示「一个 Ingress 编译成 Lua 路由并由内置 Router 服务真实流量」，
且全程通过 T0.6 conformance 金线。

## Scope（范围）
**纳入：** T0.1 · T0.2 · T0.3 · T0.4 · T0.6 · T5.1 · T5.2 · T5.3 · T5.4（T0.5 done）
**不纳入：** Layer 1–4 的真实实现、Layer 6/7、T5.5–T5.7（留到 Phase 2）。M1 允许把
API/etcd 用最小桩实现，只为给 Router 喂 Ingress。

## TODO（按 sprint 排序，每条含交付物）

### Sprint 1 — 工作空间 + multicall 骨架（T0.1 + T0.3 部分）
- Cargo workspace：`init-pro`(root bin) · `common` · `multicall`
  · `cli` · `infra`
- `argv[0]` 分派 + 别名表（`kubectl/ctr/crictl/containerd/server/agent/etcd`），
  未知→help；reexec helper（`/proc/self/exe` + `arg0`）
- `infra`：tracing、分层配置（CLI > env > file > default）、`--data-dir`、
  SIGTERM/SIGINT 优雅退出（tokio）
- 交付：`cargo build` 产出单一 `init-pro`；`scripts/multicall-selftest.sh` 全绿
- **状态：done** — commit `T0.1 + T0.3(spike)`

### Sprint 2 — CLI + bundling 管线（T0.4 先行 → T0.2）
> **Sprint 内重排：T0.4 先于 T0.2。** 理由：T0.4 无需外部下载，立即解除 T0.6
> 验收脚本（`scripts/cli-flag-parity-test.sh`）的 flag 基线阻塞；T0.2 依赖
> vendored 获取与网络/离线策略（Q6）。**M0 退出标准与 DAG 关键路径不变**；
> 仅 Sprint 内次序调整（风险 R-ordering，见下）。Q6–Q9 已在本轮锁定（见
> `decisions.md`）。

**T0.4 先行 — k3s 兼容 CLI（decision Q8 / Q9）：**
- 预 clap 层移植 k3s `configfilearg`（`--config/-c` + `INIT_PRO_CONFIG_FILE`
  + 默认 `<data-dir>/config.yaml` + `key+` 追加；`.d/` dropins / http-config
  延后并注明）；server 做无效标志剥离，agent 透传（k3s parity）
- clap-derive server/agent flag 组；accept-wired（Phase-1 子集）/
  accept-no-op-warn（其余，每进程去重 WARN）/ fatal（冲突规则）三分法，矩阵
  冻结于 `plan/00-foundation-flag-matrix.md`
- `--disable` 校验 `DisableItems`（coredns / servicelb / traefik /
  local-storage / metrics-server / runtimes）；冲突规则逐条移植
  （`--disable-apiserver`·`--disable-etcd` ✗ `--datastore-endpoint`；
  `--cluster-reset-restore-path` 需 `--cluster-reset`；`--disable-etcd` 需
  `--server`；agent 需 `--token`/`--server`）
- 交付：`scripts/cli-flag-parity-test.sh` 全绿（accept/no-op/fatal 三类）；
  `init-pro server/agent --help` 冻结基线

**T0.2 次之 — 打包管线（decision Q6 / Q7）：**
- `build.rs` 下载固定版本（containerd / runc / CNI multicall，均 Apache-2.0）
  到 gitignored `vendor/bin/`，SHA256 校验；`INIT_PRO_OFFLINE=1` 走预填
  `vendor/bin/` 离线后备（禁网）
- 许可允许列表门 + SPDX SBOM + `LICENSES/`（Q7）；GPL `k3s-root` 工具 v1 排除
- 按文件 zstd（目标 19）→ 生成 `assets.rs`；`stage()` 镜像 k3s `extract()`
  （flock → tmp → `dataverify`（`.sha256sums`+`.links`）→ 原子 rename →
  `data/current` 符号链接 → child PATH：CNI 优先、`--prefer-bundled-bin` 翻转）
- **etcd 不在此打包**（k3s 内 etcd 为进程内 Go 库；FFI 调查 = T2.1）
- 交付：`init-pro stage --dry-run` 列每个工件 + SHA256；`scripts/stage-fresh-dir-test.sh`
  全绿（字节级重算比对、`data/current` 指向、PATH 次序、幂等）；SBOM 生成 + 许可门绿

### Sprint 3 — Conformance 金线门（T0.6）
- 测试 driver：起 `init-pro server`，跑真 `kubectl`/`kube-rs`，断言 wire-level；
  CI `golden` 必过
- 空集群基线先过；后续 layer 增量挂入
- 交付：CI required check，空集群绿

### Sprint 4 — mlua coroutine↔async 桥（T5.1）★最高风险★
- mlua(LuaJIT) + 驱动器把 Lua coroutine park 到 Rust future
- yield 原语：`ngx.sleep` / cosocket `receive/send` → async I/O
- 决定 per-request vs worker-wide VM 隔离策略
- 交付：并发压测（一个请求 `ngx.sleep` 时另一请求不被阻塞）+ 延迟基线
- **Kill-criterion（R1）**：若 1 周内拿不出干净桥 → 升级 Q4 重评，不硬扛。

### Sprint 5 — HTTP phase 管线 + resty::*（T5.2 + T5.3）
- phase 钩子：`init/init_worker/rewrite/access/content/header_filter/body_filter/log/balancer`
- `ngx.*` API（`ngx.var/req/header/exit/exec/redirect/shared.DICT`）
- `resty.http`(reqwest 底) · `resty.lrucache` · `ngx.shared.DICT`；移植
  lua-resty-core/http 测试子集
- 交付：`content_by_lua` 写 body、`header_filter_by_lua` 改头，真客户端观测到

### Sprint 6 — Ingress→Lua 路由编译（T5.4，M1 spike 验收）
- informer 监听 Ingress/IngressClass/Secret/Service/Endpoints（最小桩 etcd）
- 编译器把 Ingress 编译为 Lua 路由表，`balancer_by_lua` 选上游（轮询/最少连接）
- rustls + `ssl_certificate_by_lua` SNI；默认后端 + path 类型
- 交付（M1 验收）：`kubectl apply` 一个 Ingress → `curl` host 路由命中；TLS host 工作；
  第二个 Ingress 热更新无重启

## Verify（验收 / Definition of Done）
1. `cargo build --release` → 唯一 `init-pro` 二进制
2. 全部 multicall 别名 `--help` 通过
3. CI `golden` required check 绿（空集群基线）
4. T5.1 并发桥压测通过（有量化延迟基线）
5. T5.2 phase 管线端到端可观测
6. T5.3 resty::* 子集过上游移植测试
7. **M1 spike：Ingress→Lua→真流量，含热更新**（Q5 的兑现点）

## Risks（风险与对策）
- **R1 — mlua 异步桥（Q4 成败手）**：Sprint 4 内设 kill-criterion。
- **R2 — 捆绑 Go 二进制体积/许可证**：Sprint 2 定 SBOM + 压缩策略，Apache-2.0 notice。
- **R3 — conformance protobuf 保真**：Phase 1 走 JSON-only，protobuf 推到 T1.1。
- **R4 — M1 需要 Ingress 源却依赖 Layer 1**：用最小 informer+桩 etcd。

## Align（与既有规划对齐）
- 严格映射 `index.md` 的 TODO IDs 与 DAG；Phase 1 关键路径 = DAG 的「de-risk path」：
  `T0.1 → T0.3 → T5.1 → T5.2 → T5.4`
- 不改 Q1–Q5 决策、不改 33 条 TODO 范围。
- 所有 sprint 交付物都须保 T0.6 金线绿。

## 关键路径图
```
S1 T0.1+T0.3 ─┬─> S2 T0.2+T0.4 ─┐
              ├─> S3 T0.6 (gate) ┘
              └─> S4 T5.1 ─> S5 T5.2+T5.3 ─> S6 T5.4  (M1 验收)
```
