# Layer 3 — Control Plane

Mirrors `index.md` TODO IDs **T3.1–T3.4**. The controllers, scheduler,
bootstrap/HA machinery that turn "API + storage" into a working cluster.

---

## T3.1 — kube-controller-manager 等价核心循环

- **目标 / Goal**
  The core control loops upstream ships in `kube-controller-manager`:
  `replicaset`, `deployment`, `statefulset`, `daemonset`, `endpoint`,
  `service`, `garbagecollector`, `namespace` lifecycle (k3s-relevant subset).

- **核心实现 / Core implementation**
  - `init-pro-controllers` crate; generic informer + workqueue pattern
    (kube-rs `Api::watch` + `Controller`).
  - Per-controller reconcile with upstream-identical status fields.
  - Leader election via etcd lease (T2.2) so only one server runs them.

- **验收手段 / Acceptance**
  - Golden (T0.6): scale a Deployment 1→3→1; pods converge; endpoints
    reflect membership.
  - `kubectl rollout status` parity.

- **状态 / Status** — not-started
- **证据 / Evidence** — —
- **卡点 / Blockers** — GC owner-reference graph is intricate; scope v1.
- **依赖 / Depends on** — T1.2, T2.2

---

## T3.2 — 调度器 (kube-scheduler 等价 + extender seam)

- **目标 / Goal**
  A scheduler assigning pods to nodes with the standard
  filter/score plugin model, **and a documented extender seam** so Layer 7
  (Q3) can plug in AI-agent policies later.

- **核心实现 / Core implementation**
  - `init-pro-scheduler`; plugin framework mirroring upstream scheduling
    framework (Filter/Score/Bind phases).
  - Default plugins: `NodeName`, `TaintToleration`, `NodeAffinity`,
    `PodAntiAffinity`, `ResourceFit`, `VolumeBinding`.
  - Extender seam: out-of-process gRPC/HTTP scheduler extender
    (upstream-compatible) — Layer 7 hooks here.
  - Bind via API (T1.2); leader-elected single active scheduler.

- **验收手段 / Acceptance**
  - Golden: a pod with `nodeSelector` lands only on matching nodes.
  - Extender test: a stub extender rejects/accepts a pod deterministically.

- **状态 / Status** — not-started
- **证据 / Evidence** — —
- **卡点 / Blockers** — none
- **依赖 / Depends on** — T1.2, T4.1

---

## T3.3 — bootstrap / 证书 / token 轮转

- **目标 / Goal**
  Cluster bootstrap: CA generation, serving/client cert issuance, node
  bootstrap tokens, and cert rotation (k3s `--cluster-init` / join flow).

- **核心实现 / Core implementation**
  - Self-signed CA on `--cluster-init`; join tokens (k3s
    `clientaccess.Token` parity).
  - `cfssl`/`rcgen` cert signing; kubelet client cert rotation via CSR
    auto-approval (a controller in T3.1).
  - Service-account token secrets + bound-token (v1) support.

- **验收手段 / Acceptance**
  - Golden: bring up server, join an agent via token, observe kubelet cert
    issued + rotated.
  - `openssl s_client` against API shows expected SANs/chain.

- **状态 / Status** — not-started
- **证据 / Evidence** — —
- **卡点 / Blockers** — none
- **依赖 / Depends on** — T1.3, T2.2

---

## T3.4 — HA 多 server (etcd 集群 + 选举)

- **目标 / Goal**
  Multi-server HA: embedded etcd clusters across server nodes, elected
  active controllers/scheduler, k3s `--cluster-init`/`--server` join flow.

- **核心实现 / Core implementation**
  - etcd cluster bootstrap + membership (T2.1) across N servers.
  - Per-component leader election (etcd leases): only one apiserver-etcd-
    primary set of controllers runs.
  - Proxy/load-balancing of agent→server traffic (k3s agent LB,
    `link_repos/k3s/pkg/agent/loadbalancer/`).

- **验收手段 / Acceptance**
  - Golden: 3-server cluster survives killing the leader; controllers
    continue; no split-brain.

- **状态 / Status** — not-started
- **证据 / Evidence** — —
- **卡点 / Blockers** — none
- **依赖 / Depends on** — T2.2, T3.3
