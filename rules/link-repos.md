# 规则卡: link_repos 外部钉定仓 — 6 pin + 红线

**决策码**: Q38 (pin-clone bootstrap) · Q66 (S54 anyhome 实体下线,
pin 7→6) · Q32 (openrusty) · Q33 (rdb) · Q34 (flannel-rust) · Q35
(opencoder) · Q42 (arust-workflow) · Q37 (nfsserve AUTH_SYS patch)。
**权威源**: `link_repos.lock` + `scripts/bootstrap-link-repos.sh` +
`plans/init-pro/decisions.md` Q38/Q66。本卡复述既有语义, 不得改写;
冲突时以 lock 文件与 decisions.md 为准并修复本卡。

## 1. 当前 pin 面 (6 个构建必需 pin)

| pin | 消费面 (root Cargo.toml path dep) | 备注 |
|---|---|---|
| openrusty | `openrusty-core` / `openrusty-server` | Router 数据面 (Q32); h2c 姿态钉 `4180ad3`, TLS 落地时 = pin bump + `gateway_config.rs` 前向适配 |
| rdb | `rdb` (`store`-only feature) | durable datastore `rdb://` (Q33); pin 发布在 **分支** `pin/init-pro` (上游 main 是并行重写) |
| flannel-rust | `flanneld` / `flannel-cni` | CNI (Q34) |
| opencoder | `opencoder-core` / `opencoder-session` / `opencoder-llm` | Agent 引擎 (Q35); 其 libsql 走 **crates.io** 0.9.30, 不在 pin 面 |
| nfsserve | `nfsserve` | 工作区 NFS 平面的 NFSv3 引擎 (Q66; DirFs 移植自其 `examples/mirrorfs.rs`) |
| arust-workflow | `workflow-model` / `workflow-engine` | T7.3 抽取 (Q42) |

**reference-only** (无任何 path dep 指向, bootstrap 忽略, 不得进入
构建): `link_repos/kiri` (nsrun 的 clone3 移植源, Apache-2.0, 只有
`crates/kiri-init/src/linux/` 可参照; GPL guest/kernel 树永不触碰)、
`link_repos/argo-workflows`、`link_repos/flannel`。

## 2. 硬规则 (违反即错误)

1. `link_repos.lock` 是 pin 的 **SSOT**; 增删 pin = 修改 lock + 同步
   root `Cargo.toml` + 再生成 `Cargo.lock` + decisions.md 对应 Q 码
   注记, 同一 commit 原子落地 (Q66 批范式)。**绝不提交** materialized
   树本身 (`link_repos/` 整体 gitignored)。
2. 冷克隆先跑 `scripts/bootstrap-link-repos.sh` 再 build; 该脚本是
   **幂等**的: 缺失 → clone + `checkout --detach <sha>`; 已有 git 树 →
   verify-only (HEAD==sha, 干净时自动 re-pin, 脏树拒绝 + `--force`
   逃生); dev symlink 布局 → verify-only, 永不修改。
3. **link_repos 已物化时, 禁止裸 `cargo fmt --all`** —— 会重排外部
   钉定树。fmt 只允许 member scope (如 `cargo fmt -p cli -p apiserver`);
   CI 的 fmt 步骤刻意排在 bootstrap 之前 (同因)。
4. **钉定树是只读输入**: 不 patch、不改动、不本地 fork (Q38 纪律;
   Q64 选 in-tree iptables 而非 flannel trafficmngr 即此先例)。唯一
   例外是 lock 声明的 patch: 当前仅 nfsserve 的 AUTH_SYS
   caller-identity patch, canonical 副本在本仓库
   `scripts/patches/nfsserve-auth-identity.patch`, 幂等 marker =
   `src/identity.rs`, 与消费者 commit-by-commit 前进 (Q37)。
5. `also_link` 机制 (嵌套路径相对符号链接) 是 bootstrap 的**通用
   能力但当前无 pin 使用**——唯一的消费者 (any-home 嵌套布局) 已随
   Q66 死亡; 不要为新需求复活它, 除非决策文先行。
6. pin bump = 单 lockfile diff (revert = 单 commit); bump openrusty
   前先核对 `gateway_config.rs` 的穷举构造兼容性 (Q38)。
7. nfsserve 上游演进**不再自动跟进** init-pro: `crates/cli/src/
   anyhome_vfs.rs` + `anyhome_dirfs.rs` 是 in-tree 移植 (mirrorfs @
   c00c361, BSD-3-Clause, NOTICE 归属)——上游 mirrorfs 变更需要人工
   评估再移植, 且必须保用户面契约钉死测试 (anyhome-e2e A1/P2/K1/
   P3-P5 + `anyhome_pod_mounts`) 原样全绿 (Q66)。

## 3. 修改纪律

- 动 pin 面 (增/删/换 sha) 前: 先在 decisions.md 定稿 (新 Q 码或
  修订 Q38/Q66), 再改 lock/manifest/代码——Q66 的「决策先行 → 代码
  → gates → SSOT 锁步」四段式是范式。
- `bootstrap-link-repos.sh` 的注释与 lock 的 header 教义必须与
  decisions.md 同步 (S54 曾因 any-home 教义残留需要三处联动改写)。
