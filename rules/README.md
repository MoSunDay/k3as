# rules/ — 操作规则卡 (rule cards)

本目录是**给编码会话的操作规则卡**: 从 SSOT (`plans/init-pro/decisions.md`,
`plans/init-pro/index.md`) 提炼的、容易被违反且代价高的硬约定。
规则卡**复述、绝不改写** SSOT——语义变化永远先改 decisions.md (Q 码),
再同步本卡; 二者冲突时以 decisions.md 为准并修复本卡。

## 卡片清单

| 卡片 | 内容 | 决策码 |
|------|------|--------|
| `namespace-isolation.md` | clone3/namespace 隔离画像: 默认只隔离 pid/uts/ipc/mnt/cgroup, **不**默认隔离 network 与 fs; 各 profile 的显式选择与互斥 | Q48/Q49/Q52/Q60/Q63 |
| `link-repos.md` | 外部 pinned repo (openrusty/rdb/flannel-rust/opencoder/nfsserve/arust-workflow — **6 pin**, any-home/libsql 已随 Q66 下线) 的链接、锁定与红线 | Q32/Q33/Q34/Q35/Q38/Q42/Q37/Q66 |

## 使用方式

- 新会话/子代理改 `crates/nsrun`、`crates/engine`、`RunSpec`、
  `link_repos*`、根 `Cargo.toml` 的 path 依赖前, 先读对应卡。
- 卡片是**纪律**不是设计文档: 为什么/备选方案看 decisions.md 对应 Q 码。
