# init-pro

> 一个从零开始的 **Rust** 重实现：**k3s 兼容**、完全协议标准的 Kubernetes
> 发行版 —— 以**单一 multicall 二进制**交付，内置一等公民的 **Lua 数据面**
>（一个 openresty 风格的路由器，而非 sidecar）。

[English](README.md) | **中文**

![Rust](https://img.shields.io/badge/Rust-stable%20(1.89)-ce422b?logo=rust)
![Edition](https://img.shields.io/badge/Edition-2021-orange)
![License](https://img.shields.io/badge/License-Apache--2.0-blue)
![Status](https://img.shields.io/badge/Phase%201-done%20%C2%B7%20Phase%202%20WIP-green)
![Tests](https://img.shields.io/badge/tests-326%20passing-brightgreen)
![Golden](https://img.shields.io/badge/golden-12%2F12-brightgreen)

通过 `argv[0]` 的 basename 选择行为 —— 把**同一个二进制**以任意别名部署，
符号链接部署*开箱即用*（决策 **Q1**）：

```
init-pro  server | agent | kubectl | ctr | crictl | containerd | etcd
```

整个发行版就是一个产物。

---

## 为什么是 init-pro

| | k3s | init-pro |
|---|---|---|
| 语言 | Go | **Rust**（`#![forbid(unsafe_code)]`） |
| 二进制模型 | 单一 multicall 二进制 | 单一 multicall 二进制（**Q1**） |
| Ingress / 数据面 | Traefik（独立 sidecar） | **内置 Lua 路由器** —— 一等公民的平台原语，而非插件（**Q4**） |
| 线格式 | protobuf + JSON | **v1 仅用 JSON**（**Q10**） |
| 存储 | 真正的 etcd 或 `kine`（SQLite） | `StorageBackend` trait；嵌入式默认 + 可替换后端（**Q17**） |
| 协议保真 | k8s 一致性 | **完整 k3s/k8s 一致性**（**Q2**） |

核心押注：一个**可编程的 HTTP 数据面**（openresty 的模型，移植到 Rust）位于发行版
*内部*，并且**最先**被去风险验证（**Q5**）—— 因此路由 / Ingress 在控制面完成之前，
就能跑真实流量。

---

## 状态

**Phase 1 已完成**（M0 基础 + M1 路由切片），**Phase 2 进行中** —— `storage` crate
已落地（T2.1），apiserver 现已提供**真实的 REST CRUD + watch**，基于嵌入式存储（T1.2b）。

| 层 | TODO | 内容 | 状态 |
|---:|:-----|------|:----:|
| 0 | T0.1–T0.6 | multicall、vendor/bundle、infra、CLI、golden 一致性 | ✅ 完成 |
| 1 | T1.1 | 资源模型 + API group schema + discovery builder | ✅ 完成 |
| 1 | T1.2 | APIServer：discovery（T1.2a）+ REST CRUD 与 watch（T1.2b）完成；SSA（T1.2c）延后 | 🟡 进行中 |
| 1 | T1.3 | 认证授权（kubeconfig / token / RBAC） | ⬜ |
| 2 | T2.1 | 嵌入式存储后端 + `StorageBackend` trait（Q17） | ✅ 完成 |
| 2 | T2.2 | 存储数据面接入 apiserver | 🟡 进行中 |
| 2 | T2.3 | SQLite / KINE 替代后端 | ⬜ |
| 3 | T3.1–T3.4 | controller-manager、调度器、bootstrap/证书、HA | ⬜ |
| 4 | T4.1–T4.5 | containerd/CRI、kubelet、CNI、netpol、节点隧道 | ⬜ |
| 5 | T5.1–T5.4 | Lua 桥、phase 管线、`resty.*`、Ingress→route + TLS | ✅ 完成 |
| 5 | T5.5–T5.7 | 热加载、ServiceLB、router-as-config-var | ⬜ |
| 6 | T6.1–T6.2 | HelmChart 自动部署、标准 addon | ⬜ |
| 7 | T7.1–T7.3 | AI Agent 工作负载、GPU 调度、argo-workflows | ⬜ |

> 关键路径为 **T0.1 → T0.2 → T2.1 → T2.2 → T1.2 → T3.1/T3.2 → T4.2**。
> **T0.6（golden 一致性）** 是合并门禁，而非节点：每个 TODO 都必须保持它为绿。
> 权威 SSOT 见 [`plans/init-pro/index.md`](plans/init-pro/index.md)，每个 sprint 的复盘
> 见 [`CHANGELOG.md`](CHANGELOG.md)（326 个测试通过，0 失败；golden 门禁 12/12）。

当前服务端是一个**功能完整的数据面**：API discovery + REST CRUD
（create/list/get/replace/delete/patch）+ 分块 watch 流，全部由嵌入式存储支撑，
并实现了 resourceVersion CAS。下一道门：服务端 apply 字段管理器（T1.2c）与
认证授权（T1.3）。

---

## 快速开始

```bash
# 1. 构建单一 multicall 二进制（无网络；空 embed 注册表）
cargo build --workspace --locked
#   -> target/debug/init-pro

# 2. 启动 server（discovery + REST CRUD + watch + Lua 数据面）
target/debug/init-pro server --https-listen-port 6443

# 3. 每个别名都可通过符号链接 / argv[0] 工作：
ln -sf init-pro kubectl && ./kubectl --help          # T0.1 multicall
target/debug/init-pro agent --help                   # agent 子命令
```

### 验证构建

```bash
cargo test  --workspace --locked                       # 326 个测试
cargo clippy --workspace --all-targets -- -D warnings  # 零警告
cargo fmt   --all --check                              # 干净
```

### 验收 / e2e 脚本

这些脚本运行**已构建好**的二进制，请先构建：

```bash
scripts/multicall-selftest.sh              # 每个别名响应 --help (T0.1)
scripts/cli-flag-parity-test.sh            # k3s flag 对齐 (T0.4)
scripts/graceful-shutdown-test.sh          # SIGTERM 干净退出 (T0.3)
scripts/router-coroutine-selftest.sh       # coroutine↔async 桥 (T5.1)
scripts/apiserver-discovery-parity-test.sh # 与 k8s 逐字节对齐 (T1.2a)
scripts/golden-conformance.sh              # 不可变 wire 基线 (T0.6)
scripts/stage-fresh-dir-test.sh            # 运行时 stage() bundling (T0.2)
```

---

## 内置 Lua 路由器（招牌特性）

一个真正的 openresty 风格数据面 —— phase 管线、`resty.*`、Ingress 编译、反向代理、
TLS —— 全部位于二进制**内部**，并可通过 [`mlua`](https://crates.io/crates/mlua) 用 Lua 驱动：

```
client request
   │
   ┌───▼──────────────────────────────────────────────┐
   │  Router VM (single mlua VM per worker, !Send)     │  Q12/Q13
   │  phase pipeline: rewrite → access → content →     │  Q14
   │    header_filter → body_filter → log  (+ init_worker)
   ├──────────────────────────────────────────────────┤
   │  ngx.*      req / header / status / var / exec   │
   │  resty.*    http · lock · random · string ·      │  T5.3
   │             lrucache · ngx.shared.DICT           │  Q15
   │  cosocket   ngx.socket.tcp (async bridge)        │
   ├──────────────────────────────────────────────────┤
   │  Ingress→Lua compiler → round-robin Balancer →   │  T5.4
   │  reverse proxy → upstream resolver               │
   │  TLS termination + SNI (rustls `ring`, Q16)      │
   │  hot-reload seam (no-restart config swap)        │
   └──────────────────────────────────────────────────┘
       │
   upstream pods
```

coroutine↔async 桥（Q12）让 Lua 协程能在 Rust 的 `await` 点上 `yield`，运行于
Tokio `LocalSet` 之上，从而在 Rust 中重现 openresty 的协作式并发。详见
[`features/router-data-plane.md`](features/router-data-plane.md)。

---

## 工作区布局

10 个 crate，扁平的 `src/*.rs` 模块，在 crate 根用 `pub use` 再导出：

| Crate | 职责 | 层 |
|-------|------|:--:|
| [`init-pro`](crates/init-pro) | 单一二进制；`argv[0]` 分派 + `build.rs` 资产打包 | 0 |
| [`multicall`](crates/multicall) | 别名表 + reexec + peer 桩 | 0 |
| [`vendor`](crates/vendor) | 固定版本上游产物获取 + SHA-256 校验 + SPDX SBOM | 0 |
| [`infra`](crates/infra) | tracing、分层配置、优雅关闭 | 0 |
| [`cli`](crates/cli) | clap CLI + 子命令 + k3s flag 对齐过滤 | 0 |
| [`common`](crates/common) | 共享原语：embed 描述符 + `version()` | 0 |
| [`api`](crates/api) | 资源模型、schema 注册表、discovery builder、`init-pro.io` CRD | 1 |
| [`apiserver`](crates/apiserver) | discovery + REST CRUD + watch（基于 `StorageBackend`，T1.2） | 1 |
| [`storage`](crates/storage) | `StorageBackend` trait + 嵌入式 etcd 兼容存储 | 2 |
| [`router`](crates/router) | 内置 Lua 数据面：phase 管线、`resty.*`、Ingress→route、负载均衡、反向代理、TLS | 5 |

---

## CI 与容器

CI（[`.github/workflows/ci.yml`](.github/workflows/ci.yml)）在每次 push/PR 时运行：
**fmt `--check` → clippy `-D warnings` → `cargo test --locked` → debug 构建 → 完整
e2e 套件**，外加一个独立的 **`INIT_PRO_EMBED=1` 打包作业**
（`stage-fresh-dir-test.sh`）。

多阶段 [`Dockerfile`](Dockerfile) 在一个精简镜像中构建单一二进制
（rust:1.89 构建器 → debian:bookworm-slim 运行时），并安装每个 multicall 符号链接：

```bash
docker build -t init-pro .                 # 加 --build-arg EMBED=1 以内嵌 peer
docker run --rm -p 6443:6443 init-pro server
```

---

## 构建标志（`build.rs`）

| 标志 | 效果 |
|------|------|
| *（默认）* | **无网络** + **空 embed 注册表** —— 快速开发循环与 `cargo test` |
| `INIT_PRO_VENDOR=1` | 下载固定版本的上游 peer（`containerd`/`etcd`/…） |
| `INIT_PRO_OFFLINE=1` | 完全禁止网络（CI/发布加固） |
| `INIT_PRO_EMBED=1` | 将下载的 blob 内嵌进二进制（发布镜像） |

CI/发布必须使用 `--locked`；依赖均为 caret-only。

---

## 架构决策（ADR）

共 17 个锁定决策，见 [`plans/init-pro/decisions.md`](plans/init-pro/decisions.md)。
其中承重的几个：

| ADR | 决策 |
|-----|------|
| **Q1** | 单一 multicall 二进制（`argv[0]` 分派） |
| **Q2** | 完整 k3s/k8s 协议一致性（不另起炉灶造 API） |
| **Q4** | 路由器是一等公民的平台原语，而非插件 |
| **Q5** | *最先*去风险验证路由器（M1 垂直切片） |
| **Q10** | v1 仅用 JSON 线格式（不用 protobuf） |
| **Q12–Q14** | 路由器 VM 模型、coroutine↔async 桥、按 phase 的协程 |
| **Q16** | 加密 provider = `ring`（而非 `aws-lc-rs`） |
| **Q17** | 存储 = trait 之后的纯 Rust 嵌入式存储（而非 etcd FFI） |

---

## 面向 agent 的仓库本地记忆

如果你是在本仓库工作的 AI 编码 agent，从这里开始：

- [`agents.md`](agents.md) —— 入口：SSOT 指针、构建门禁、约定、状态。
- [`features/`](features/) —— 特性卡片（`discovery-api`、`router-data-plane`、
  `storage-layer`）+ 以特性为中心的变更日志。
- `plans/init-pro/` —— **SSOT**：`index.md`（状态表）、`plan/*.md`
  （按层细节）、`decisions.md`（Q1–Q17）。

**切勿与 SSOT 矛盾。** 当你改变语义时，必须同步更新 `index.md` +
对应的 `plan/*.md` + `decisions.md`。

---

## 强制约定

- 每个 lib crate 都有 `#![forbid(unsafe_code)]`。
- 纯函数式 Rust 风格：`struct` + `impl` + trait 可以；不要 OOP 继承。
- 扁平的 `src/*.rs` 在 `lib.rs` 中用 `pub mod` 声明；子目录用 `mod.rs`；
  在 crate 根用 `pub use` 再导出。
- 文件大小：新文件 ≤ 400 行，迭代中文件 ≤ 800 行。
- v1 **仅用 JSON** 线格式（不用 protobuf）。
- 不得硬编码任何 secret/API key/token；未经明确许可不得执行破坏性 DB 操作。
  注释需引用任务 ID（`T1.1`、`T5.4`）与决策码（`Q10`、`Q17`）。

---

## 许可证

Apache-2.0。打包的上游 peer 保留各自许可证；构建会生成一份
[SPDX-2.3 SBOM](LICENSES/)（T0.2）。
