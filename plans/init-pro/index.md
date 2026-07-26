# init-pro — Index (SSOT)

This file is the **single source of truth** for all 33 TODOs. The
`plan/<layer>.md` files mirror the same TODO IDs (same content, per-layer
grouping for easier editing). **Edit both in lock-step.**

Legend & rules: see `README.md` §6 and `template/TODO.md`.

## Status table

| ID | Layer | Title | Status | Depends on |
|----|-------|-------|--------|------------|
| T0.1 | 0 | 单一二进制 multicall crate 骨架 | done | — |
| T0.2 | 0 | 构建与 bundling pipeline | in-progress | T0.1 |
| T0.3 | 0 | 公共基础设施 crate (log/config/signal) | done | T0.1 |
| T0.4 | 0 | CLI: multicall + k3s 兼容 flag | done | T0.1, T0.3 |
| T0.5 | 0 | 规划体系与文档中枢 | done | — |
| T0.6 | 0 | 协议兼容性测试基线 (golden conformance) | not-started | T0.1 |
| T1.1 | 1 | 资源模型与 API group schema | not-started | T0.3 |
| T1.2 | 1 | APIServer 核心 (REST + kubectl 真实交互) | not-started | T1.1, T2.2 |
| T1.3 | 1 | 认证授权 (kubeconfig/token/RBAC) | not-started | T1.1 |
| T2.1 | 2 | etcd embed/FFI/子进程 bundling | not-started | T0.2 |
| T2.2 | 2 | etcd v3 数据面 = APIServer storage backend | not-started | T2.1, T1.1 |
| T2.3 | 2 | SQLite/KINE 兼容替代后端 | not-started | T2.2 |
| T3.1 | 3 | kube-controller-manager 等价核心循环 | not-started | T1.2, T2.2 |
| T3.2 | 3 | 调度器 (kube-scheduler 等价 + extender seam) | not-started | T1.2, T4.1 |
| T3.3 | 3 | bootstrap / 证书 / token 轮转 | not-started | T1.3, T2.2 |
| T3.4 | 3 | HA 多 server (etcd 集群 + 选举) | not-started | T2.2, T3.3 |
| T4.1 | 4 | containerd bundling 与 CRI 实现 | not-started | T0.2 |
| T4.2 | 4 | kubelet 等价 (pod 生命周期 + status) | not-started | T4.1, T3.2 |
| T4.3 | 4 | CNI/网络 (flannel 等价 + ServiceLB L4) | not-started | T4.2 |
| T4.4 | 4 | 网络策略 (netpol 等价) | not-started | T4.3 |
| T4.5 | 4 | 节点注册/心跳/代理隧道 | not-started | T4.2, T1.2 |
| T5.1 | 5 | mlua + coroutine↔async 桥 | not-started | T0.3 |
| T5.2 | 5 | HTTP 管线 + phase hooks | not-started | T5.1 |
| T5.3 | 5 | resty::* 等价标准库 | not-started | T5.1 |
| T5.4 | 5 | 内置 Router 核心 + Ingress→Lua 路由编译 | not-started | T5.2, T5.3, T1.1 |
| T5.5 | 5 | 热加载 / 动态配置 (no-restart reload) | not-started | T5.4 |
| T5.6 | 5 | ServiceLB (L4/LB 数据面) | not-started | T5.4, T4.3 |
| T5.7 | 5 | 内置 Router 作为平台配置变量 | not-started | T5.4, T4.5 |
| T6.1 | 6 | manifest 自动部署 (HelmChart auto-deploy) | not-started | T1.2, T3.1 |
| T6.2 | 6 | 标准 addon (CoreDNS/local-storage/metrics-server) | not-started | T6.1 |
| T7.1 | 7 | AI Agent workload CRD + scheduler extender | not-started | T3.2, T1.2 |
| T7.2 | 7 | GPU/资源调度与策略 | not-started | T7.1, T4.2 |
| T7.3 | 7 | argo-workflows 最小化整合 (Workflow/CronWorkflow 一级公民内置) | not-started | T1.2, T3.1 |

**Counts:** 33 TODOs · Layer0=6 · Layer1=3 · Layer2=3 · Layer3=4 · Layer4=5 · Layer5=7 · Layer6=2 · Layer7=3.

---

## DAG (critical path)

```
T0.1 ─┬─> T0.2 ─┬─> T2.1 ─> T2.2 ─┬─> T1.2 ─┬─> T3.1 ─> T6.1 ─> T6.2
      │         │                 │          ├─> T3.2 ─> T7.1 ─> T7.2
      ├─> T0.3 ─┤                 │          ├─> T3.3 ─> T3.4
      │         └─> T4.1 ─> T4.2 ─┼─> T4.3 ─> T4.4
      ├─> T0.4                      │          └─> T4.5
      ├─> T7.3 <─ T3.1 <─ T1.2      │   (argo-workflows first-class)
      └─> T0.6 (golden gate,        │
                every TODO must     T1.1 ─┬─> T1.3
                keep it green)      T2.3 <─┘
                                         T5.1 ─> T5.2 ─┐
                                         T5.3 <───────┘─> T5.4 ─┬─> T5.5
                                                                  ├─> T5.6
                                                                  └─> T5.7
```

- **Critical path (platform):** T0.1 → T0.2 → T2.1 → T2.2 → T1.2 → T3.1/T3.2
  → T4.2 → end-to-end cluster (M3).
- **De-risk path (Q5):** T0.1 → T0.3 → T5.1 → T5.2 → T5.4 (M1 spike).
- **T0.6** is a gate, not a node: any TODO merging must keep it green.
- **Sprint 2 (T0.2 + T0.4):** T0.4 = **done**. T0.2 = **in-progress** — B1 (pinned artifact acquire, Q6: `init-pro-vendor` crate + `build.rs` + `vendor/versions.toml`, SHA-256 verify, 3 acquire modes incl. `INIT_PRO_OFFLINE=1` air-gap) done; embed/stage/SBOM (B2–B5) next. Frozen flag matrix: `plan/00-foundation-flag-matrix.md`; ADRs Q6–Q9 in `decisions.md`.

---

## Field reference (see `template/TODO.md`)

Each TODO carries 7 fields: **目标 / 核心实现 / 验收手段 / 状态 / 证据 / 卡点 / 依赖**.

Full detail for every TODO lives in `plan/00-foundation.md` …
`plan/07-agent-scheduling.md` (TODO IDs strictly mirror this file).
