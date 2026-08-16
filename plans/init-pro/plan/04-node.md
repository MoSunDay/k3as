# Layer 4 — Node / Agent

Mirrors `index.md` TODO IDs **T4.1–T4.5**. The node side: bundled
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

- **状态 / Status** — in-progress (Scope A done, Sprint 17)
- **证据 / Evidence** — kubelet 53 / apiserver 51 / runtime +17 tests;
  golden G25 (27/27 with vendor + `cc`); Q27 ADR
- **卡点 / Blockers** — Scope B/C remainder: probes
  (readiness/liveness/startup), volumes (emptyDir/configMap/secret
  first; CSI later), real image pull path, exec/logs/attach, local
  deploy.
- **依赖 / Depends on** — T4.1, T3.2

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

- **验收手段 / Acceptance**
  - Golden: pod-to-pod across nodes; ClusterIP reachable; LoadBalancer
    Service gets an external IP and routes.

- **状态 / Status** — not-started
- **证据 / Evidence** — —
- **卡点 / Blockers** — Dataplane choice (iptables vs eBPF) — decide here.
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

- **状态 / Status** — not-started
- **证据 / Evidence** — —
- **卡点 / Blockers** — none
- **依赖 / Depends on** — T4.2, T1.2
