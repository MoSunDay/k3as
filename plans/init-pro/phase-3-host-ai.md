# Phase 3 — Agent sandbox + host-native AI execution plan (Q59–Q63)

> **Status: active forward plan, locked 2026-11-19.** This is the successor to
> the historical/archived `phase-2-implementation.md`. TODO status and counts
> remain authoritative in `index.md`; detailed contracts live in
> `plan/04b-builtin-engine.md`, `plan/07-agent-scheduling.md`, and Q59–Q63 in
> `decisions.md`. This replan is documentation-only: it does not mark any new
> implementation done and does not rewrite completed Sprint history.

## 0. Sprint 47 (node04-llama-k3as) — P1/P2/P3 fake-scope landed

S47 landed the implementation slice for the inference track; **no TODO was
closed** (real-hardware closure criteria in §4-§6 are unmet):

- **T4.15 in-progress**: strict-triple host-direct selection (`host_direct.rs`),
  nsrun/engine host-root child setup (no NEWNET, no `pivot_root` for `/`,
  host `/dev`+`/sys` untouched, fresh `/proc`, private propagation first),
  zero image actions for workload AND pause (HD2), broken-triple refusal
  (HD5). Evidence: builtin-hostroot-e2e H1-H8 + **HD1-HD5**, engine-selftest
  33/33, builtin-runtime B1-B13 + N1-N5, golden G90-G96. Remaining: node04
  on-site HD verification.
- **T7.2 in-progress (fake scope)**: `--gpu-inventory auto|none|fake:<res>:<n>`,
  capability-gated capacity/allocatable publication, persistent
  whole-GPU ledger (`engine/gpu-ledger.json`, Pod-UID × container, stable
  across restarts, released only after teardown), scheduler assigned-pod
  accounting (+ JSON-pointer `~0/~1` fix), `CUDA/HIP_VISIBLE_DEVICES`
  injection, `status.initProRuntime` (gpuLedger + released records)
  heartbeat contract, hard no-containerd guard. GPU1-GPU5 unit-tested;
  **GPU6 real per-vendor evidence outstanding** — T7.2 stays open.
- **T7.6 in-progress (fake scope)**: Recreate teardown-hold state machine
  (new RS held at 0 until per-UID teardown + ledger-release confirmation,
  stale acks defeated by `init-pro.io/teardown-since` + fresh node status;
  Deployment `OldRuntimeTeardownPending`), offline fake-OpenAI e2e
  `scripts/inference-fake-e2e.sh` **11/11 PASS** (INF1/2/3/5 fake scope:
  empty Endpoints before ready, `/health` readiness, models/chat/SSE
  `[DONE]` through NodePort, env injection, no model-aware routing, no
  image pull), and `plans/init-pro/ops/` (node04 inventory template,
  switch/rollback runbook, parameterized manifests). **INF6 real
  llama-server/GGUF/GPU acceptance outstanding** — T7.6 stays open.
- Admission (S47 cross-cutting): invalid explicit `imagePullPolicy`
  rejected, GPU quantities integral + request<=limit, host-direct template
  requires `strategy.type: Recreate`, GPU templates require the literal
  `/` annotation; RS/DS/STS now propagate template annotations to created
  pods (was the S47-reported bug). S47 review F2 moved the
  GPU-requires-`/` rule onto the shared `validate_pod` path (bare Pods +
  RS/STS/DS templates all covered, Deployment keeps only the Recreate
  rule) and review F1 added: a GPU-requesting container explicitly setting
  `CUDA_VISIBLE_DEVICES`/`HIP_VISIBLE_DEVICES` is 422-rejected (the
  platform injects it from the GPU ledger, Q61). Golden `phase3-v1` =
  G90-G98.
- Gate status after the S47 review fixes (F1-F5, same tree):
  build/test/clippy/fmt/golden/parity/selftest/builtin-e2e/resources-e2e
  all green — workspace tests **1327 passed / 0 failed** (the two
  `external_stub` failures were root-caused to host data-dir residue:
  prior e2e runs staged real peers under `/var/lib/init-pro`, so the
  multicall exec'd them before the stub; NOT `/usr/bin/ctr`, which the
  staged-peer lookup never consults. The test now pins `INIT_PRO_DATA_DIR`
  at an empty tempdir — F4), `golden-conformance.sh` 60/60 (G90-G98).
- S47 评审修复 F3——teardown-hold release-ack 确认增加 60s 时钟偏差
  margin（防节点快钟提前放行、慢钟仅延迟不锁死），并文档化 NTP ≤ margin
  前置假设；等待消息携带两侧观测时间戳。

## 1. Current baseline

- The SSOT contains **54 TODOs: 38 done, 2 in-progress, 14 not-started**.
  Layer 4 has 15 cards (T4.1–T4.15); Layer 7 has 7 cards (T7.1–T7.7).
- k3as is a **multi-tenant Agent sandbox platform**. One tenant per cluster with
  tenant-dedicated nodes is **necessary but not sufficient** for hostile
  multi-tenant support; §7 lists the additional audited/tested isolation controls.
  Kubernetes Namespaces are not tenant security boundaries. Q63 Agent action
  containment is distinct, and Q60 host-direct is only an ordinary-inference
  performance profile, not containment or a cross-tenant boundary.
- T4.11–T4.14 are done (Q48–Q55): the builtin engine, kubelet wiring, image
  and host-net paths, host-directory rootfs, and ro/rw host-mounts already
  exist. T4.15 extends those cards; T7.7 reuses them. Neither reopens them.
- T3.1 and T3.2 are done and are reused by the inference and GPU cards. T7.1
  Agent execution remains done under Q35/Q37 and is a dependency of P0 T7.7,
  which extends it without reopening it. T7.7 does not depend on inference.
- T8.5 remains **done (Sprint 46, Q58)** and Q58 remains **LOCKED**. The
  delivery-plane history is not changed by this Phase 3 replan.
- T4.3 and T5.6 remain in-progress for their recorded remainders. Unfinished
  platform capabilities are listed in §9 and remain tracked.
- The global engine default remains `containerd` (Q51). Every host-direct
  inference e2e and workload explicitly selects `builtin`; no global default
  flip is hidden in the rollout. Sandboxed Agents follow Q63 instead and never
  fall back to direct processes, host root/network, or containerd.

### Goal and critical sequence

Deliver Agent action containment first, then an independent ordinary-inference
track using standard Kubernetes objects. Q60 host root/network/device reuse is
only a performance profile inside a tenant-dedicated cluster; Agent workloads
use Q63 instead. The branches do not acquire cross-dependencies:

```text
Phase 3 P0 (first)
T7.1 done + T4.14 done ──> T7.7 / Q63
                           default-deny action gate + Linux Agent sandbox
                           per-attempt provider broker; no unsafe fallback

Phase 3 P1                    Phase 3 P2                Phase 3 P3
T4.14 done ──> T4.15 / Q60 ──> T7.2 / Q61          ──> T7.6 / Q62
               host-direct       whole-GPU scheduling     llama.cpp inference
               performance       stable logical exclusive Deployment + NodePort
               profile           allocation               OpenAI-compatible API
```

No stage may be marked done on another stage's fake. T7.7 must independently prove
policy and containment; T4.15 must work without a GPU. Fake results close only
explicitly labelled fake subscopes: recorded evidence for every supported GPU vendor
is mandatory for T7.2, and a real pinned llama-server/local read-only GGUF/real-GPU
run with proof of accelerator execution is mandatory for T7.6 (§8).

## 2. Q59 supported Kubernetes protocol profile

Q59 supersedes/narrows Q2's **full-conformance forward target**. It does not
rewrite Q2, weaken completed APIs, or permit a custom dialect. There is no full
Kubernetes conformance claim.

### Support contract

1. **Discovery + golden matrix define support.** A supported API must be
   published in discovery and covered by the versioned T0.6 golden matrix.
   Absence from either is not a silent compatibility promise.
2. **Upstream semantics remain mandatory inside the profile.** Supported
   resources retain upstream GVK/GVR, JSON representation, CRUD, watch, SSA,
   status, and error behavior. A narrower API set does not permit false
   resourceVersions, invented status, or successful no-op writes.
3. **Priority clients and workloads are explicit.** Test real `kubectl` and
   `kube-rs`, prioritizing ordinary `apps/v1` Deployment plus `core/v1`
   Service, Pod, ConfigMap, and Secret workflows. Existing built-in resources
   remain governed by the same discovery + matrix rule.
4. **Q10 remains unchanged.** The v1 wire is JSON-only. Protobuf is outside
   the near-term profile rather than partially or silently supported.
5. **Unsupported surfaces fail explicitly.** Discovery must not advertise
   fictional support, unsupported operations must return documented
   Kubernetes-style errors, and the support matrix must record the limitation.
   Never return fake success for an operation that was not performed.
6. **T0.6 stays done and gates every stage.** Its green result covers the
   supported subset only. Newly claimed APIs must extend the matrix and its
   client/golden tests before they are advertised.
7. **Append versioned `phase3-v1` golden cases at implementation; never rewrite historical IDs.** Cover strict host-direct triple positives and missing/wrong/nonliteral-`/` negatives; no containerd fallback; zero workload/pause image actions for omitted/defaulted and every valid pull policy on create/restart/reconcile, plus invalid-policy rejection.
   Cover compatible GPU publication/accounting/exclusivity/restart/release and incompatible-backend, insufficient/invalid-resource and stale-nonzero-publication negatives; Recreate runtime/GPU ordering plus stalled teardown/release; invalid Deployment strategy rejection; inference NodePort reachability and empty endpoints.
   Use real kubectl/kube-rs for API/client assertions and privileged/runtime traces as needed. G70/G71/ST1-ST6 are generic data-plane evidence only; T7.6 still proves inference Pod Service/Endpoints lifecycle instead of repeating generic proxy tests.

Dynamic CRD registration, admission webhooks/controllers, API aggregation,
protobuf, full RBAC, and other broad control-plane surfaces are not near-term
unless a concrete target ecosystem workload requires them. Such a requirement
must come with discovery, explicit failure behavior, and golden coverage; it
must not be smuggled into the Phase 3 stages.

## 3. Phase 3 P0 — T7.7 Agent action interception + Linux sandbox (Q63)

### Deliverables

- Extend done T7.1/Q35/Q37 without reopening it. T7.7 depends on T7.1 + T4.14 only and is independent of T4.15/T7.2/T7.6. Reuse existing nsrun/clone3/cgroups/engine lifecycle as appropriate; add no second runtime, daemon, syscall, or container stack.
- Stage delivery explicitly: a mediation-only central gate/provider broker is a development milestone, **not Q63 containment or supported hardened mode**, and cannot close T7.7. Hardened requests fail Pending/Failed while containment is unavailable.
  Hardened completion puts Agent, opencoder, all ordinary/custom/MCP/ssh tools, all subagents, and descendants under the same action gate and attempt Linux identity/namespaces/cgroups/cleanup; none may escape.
- Establish a node/operator-owned **default-deny** baseline. AgentSpec,
  workspace config, resumed sessions, model/agent changes, and subagents may
  only narrow it, never broaden, replace, or disable it. Recompute effective
  policy after resume and configuration changes; broadening attempts receive
  deterministic denials and audit events.
- Route ordinary tools, early task/subagent dispatch, MCP, ssh, and custom
  tools through one central tool-dispatch gate. Subagents inherit exactly the
  same effective policy and recurse through the same gate. Every denied action
  produces a deterministic denial plus an audit event that contains no secret.
- Enforce every Agent attempt with a private **non-host** rootfs/mount namespace, fresh netns, non-root uid/gid, `no_new_privs`, and empty capabilities. Permit only operator-allowlisted selective host binds; runtime/tool binds are read-only and only designated workspace/scratch are writable, which Agent config cannot broaden.
  Use a fresh minimal `/dev` with safe nodes only, never host `/dev`. Expose no host container/engine/Docker/CRI/other runtime-control sockets; the per-attempt broker Unix socket is the sole intentional host IPC exception. Literal host `/`, host netns, and Q60 host-direct are forbidden for Agents.
- Require bounded cgroup v2 CPU, memory, and PIDs limits for every attempt.
  Keep Agent, opencoder, tools, and subagents under one lifecycle and reliably
  clean all descendants on success, failure, timeout, or cancellation before
  reclaiming cgroups, mounts, and sockets.
- Provide provider access only through a node-side operation-allowlisted
  broker over a per-attempt Unix socket. The broker permits only configured
  provider operations and injects credentials upstream; it is not a generic
  proxy and grants no arbitrary egress. Agent/opencoder/tool/subagent
  environments receive no provider credentials.
- Q63 supersedes Q35/Q37 child-env credential transport **only for the
  sandboxed Agent path**, preserving their done status and historical evidence.
  If any required containment or broker capability is unavailable, leave the
  Agent Pending/Failed with an actionable reason; containment and credential
  confinement are fail-closed. Never fall back to a direct process, host root,
  host network, or containerd.

### Gates

- **AS1 policy/inheritance:** unit/fake tests cover narrowing, override
  resistance for every configuration/resume/model/agent/subagent source, all
  tool and early-dispatch entrances, identical subagent policy, deterministic
  denial, and audit emission.
- **AS2 filesystem/identity:** privileged Linux integration proves non-host rootfs,
  non-root/empty-caps/`no_new_privs`, allowed/denied binds, read-only runtime/tool
  binds, denied writes outside workspace/scratch, fresh minimal `/dev`, and absent
  host container/engine/Docker/CRI/other runtime sockets; host `/`, host `/dev`,
  and Q60 selection are unreachable.
- **AS3 network/broker:** prove a fresh netns, no arbitrary egress, allowlisted
  provider operations only, per-attempt socket revocation, and no generic-proxy
  behavior using a fake local provider only.
- **AS4 resources/descendants:** verify mandatory cgroup v2 CPU/memory/PIDs limits;
  Agent/opencoder/tools/subagents/descendants share the gate, attempt identity,
  namespaces, cgroups, and cleanup, with no escape and complete cleanup on every
  success/failure/timeout/cancellation path.
- **AS5 secret canaries:** credentials canaries are absent from Agent,
  opencoder, tool, and subagent env, argv, logs, and audit records; only the
  broker can inject them upstream. Tests require no external service.
- **AS6 no fallback/regressions:** forced sandbox/broker failures remain
  Pending/Failed with actionable reasons and create none of the four forbidden
  fallbacks. T0.6 and existing build/test/clippy/safety/Agent/engine gates stay
  green. Delivery may be staged, but all AS1–AS6 evidence is required to close
  T7.7.

## 4. Phase 3 P1 — T4.15 builtin host-direct execution (Q60)

### Deliverables

- Select host-direct **before kubelet sync image planning**, requiring all three
  signals: `--container-engine=builtin`, `--builtin-rootfs=host`, and literal Pod
  annotation `init-pro.io/rootfs: /`. `image` is retained declarative/wire metadata
  only on this path; literal `/`, not `image`, selects host rootfs.
- Add conditional nsrun/engine child setup for literal host root:
  - retain clone3 NEWNS, NEWPID, NEWUTS, NEWIPC, and NEWCGROUP;
  - do not add NEWNET (Q52);
  - on create/restart/reconcile, emit no `Action::EnsureImage` for workload or
    pause/sandbox;
  - perform no pull, presence check, ref/CAS/layer resolution/materialization,
    image-config merge, layer copy/merge, or `pivot_root` for `/`;
  - accept omitted/defaulted and valid Always/IfNotPresent/Never as behaviorally
    irrelevant; reject an invalid explicit policy with Kubernetes-style validation,
    never silently default it;
  - make mount propagation private/safe before child-specific mounts;
  - mount/handle `/proc` for the new PID namespace without changing the host;
  - preserve host `/dev` visibility only for ordinary inference on nodes
    dedicated to one tenant; never select this path for an Agent.
- Preserve T4.14 ro/rw host-mounts, Q55 ordinary host-directory presence-only
  behavior, and T4.9 CPU/memory cgroup limits. Existing host-directory and
  image-rootfs paths must remain byte/behavior compatible.
- Emit actionable Pod events/status for invalid selections or unavailable
  host-direct capability. A host-direct or GPU request must never run through
  containerd, even as an explicit fallback. Q51's generic boot-probe fallback remains
  only a generic node behavior, not permission to misexecute these requests.
- Document and enforce the tenant-dedicated deployment boundary and the
  separate Agent action-containment boundary in §7. Host `/`, host `/dev`, and
  host netns are acceptable only for ordinary inference in that deployment;
  nodes must never be shared across tenants.

### Gates

- **HD1 selection/golden:** `phase3-v1` covers strict-triple positives and every
  missing/wrong-selection or nonliteral-`/` negative, with unchanged Q51/Q55 defaults.
- **HD2 direct root/no images:** privileged runtime traces observe a host marker at
  `/` and prove no workload/pause image action or processing on create/restart/
  reconcile for omitted/defaulted and all valid pull policies; invalid explicit
  policy is rejected and ordinary Q55 host-directory presence-only is unchanged.
- **HD3 namespaces:** inspect the namespace set, verify no NEWNET, verify PID
  isolation through `/proc`, and verify mount changes do not propagate back.
- **HD4 existing controls:** expose a precreated fake device node under `/dev`;
  re-run ro/rw host-mount and CPU/memory limit tests. No real GPU is required.
- **HD5 explicit failure:** force engine/probe/create failures and assert a
  clear event/status plus zero containerd container creation for host-direct
  or GPU requests.
- **HD6 regressions:** existing engine H/I/N batteries, T0.6 supported-subset
  golden, workspace tests, clippy, and safety/single-thread gates stay green.

Phase 3 P1 closes only with actual implementation evidence in T4.15's card;
planning text itself is not evidence.

## 5. Phase 3 P2 — T7.2 direct host whole-GPU scheduling (Q61)

### Deliverables

- Publish `nvidia.com/gpu`/`amd.com/gpu` as total current eligible whole-device inventory in both capacity and allocatable. Only compatible builtin host-direct nodes (builtin + `--builtin-rootfs=host` + successful host-direct and device-capability checks) may publish nonzero; containerd/image-rootfs/failed-probe/incapable nodes omit or report zero despite physical devices.
  GPU Pods still need literal `init-pro.io/rootfs: /`; eligibility does not relax Q60's triple.
- One canonical Node status constructor always receives current eligible inventory and serves registration, every heartbeat, agent restart, and every full rebuild (heartbeat replaces full status). Assigned-Pod accounting stays in the scheduler/allocation ledger.
  Test every constructor path and inventory/capability/backend transition, including no stale nonzero allocatable.
- Fix generic extended-resource behavior in the scheduler/API path:
  - JSON Pointer escaping maps `~` to `~0` and `/` to `~1`;
  - absent extended-resource capacity means zero;
  - snapshots account for all assigned Pods that still hold resources;
  - requests and limits are integral and, after standard defaulting, equal.
- Implement whole-GPU logical exclusive allocation. Use a durable node-side
  ledger keyed by **Pod UID + container name**, recording resource and physical
  device ID. Restore existing allocations after an agent restart before
  admitting new work; reserve atomically to prevent double booking.
- Inject `CUDA_VISIBLE_DEVICES` for NVIDIA or `HIP_VISIBLE_DEVICES` for AMD
  from the selected device. Keep the same selection across retries and agent
  restarts; release only after container teardown is complete.
- Schedule the first slice only onto compatible builtin host-direct nodes in a
  tenant-dedicated cluster. Unsupported backends and insufficient capacity
  produce explicit conditions; GPU requests never fall back to containerd, even
  explicitly. This remains ordinary-inference allocation, not Agent action
  containment.
- Provide fake-inventory CI and an explicit hardware-required harness mode.

### Gates

- **GPU1 resource correctness:** unit tests cover pointer escaping, absent=zero,
  assigned-Pod accounting, integral request/limit validation, and terminal Pod
  handling.
- **GPU2 exclusivity:** with one fake GPU, one Pod runs and a competing Pod
  stays Pending; concurrent creation cannot reserve the same device twice.
- **GPU3 stability:** repeated syncs and an agent restart preserve the same
  Pod UID/container → device mapping and visible-device environment variable.
  Deleting and recreating a Pod creates a new UID and cannot inherit stale
  ownership blindly.
- **GPU4 release:** allocation survives until teardown finishes, then becomes
  available to a subsequent Pod; crash/retry loops cannot leak or double book.
- **GPU5 no fallback/stale publication:** containerd-only or otherwise incapable
  nodes omit/zero both GPU fields, produce a clear Pending/Failed outcome, and never
  execute elsewhere; capability/backend transitions cannot leave stale nonzero
  allocatable. Include insufficient and invalid resource negatives.
- **GPU6 real-hardware closure:** fake green closes only labelled fake subscopes.
  Recorded real-driver/device evidence for inventory, selected environment,
  exclusivity, restart recovery, and teardown release is mandatory for every vendor
  claimed supported (NVIDIA+AMD means both); otherwise narrow support/Q59 matrix.
  Without it T7.2 remains open even if all fake tests pass.
- T0.6 and all standard quality gates remain green; fake results are recorded
  as fake, not as proof of physical GPU correctness.

External device-plugins, containerd GPU/device-plugin support, fractional GPU,
MIG, time-slicing, Agent-specific GPU integration, and broader policy scheduling
are deferred. T3.2 and T7.1 remain done.

## 6. Phase 3 P3 — T7.6 llama.cpp-first inference (Q62)

### Deliverables

- Supply an ordinary `apps/v1` Deployment → Pod and `core/v1` NodePort Service
  workload contract. No Agent, no new CRD, and no automatic model deployment
  controller are involved.
- The Pod explicitly uses T4.15 host-direct and T7.2 standard GPU requests. Current Recreate is insufficient: scale the old RS to zero, then hold the new RS at zero until actual runtime teardown of all old containers/sandboxes and GPU allocation release are explicitly acknowledged/completed.
  RS spec/status zero, deletionTimestamp, terminating state, or API Pod absence is not proof. The implementation-owned completion signal must represent node/runtime + allocation ledger completion and correlate to old Pods/attempts so stale acks cannot unblock.
  A stall leaves replacement at zero with actionable status until completion. Reject invalid Deployment `strategy.type`; never treat it as RollingUpdate.
- Run an operator-preinstalled, version-pinned `llama-server` from the host.
  Prestage an immutable GGUF on host storage and expose it with a read-only
  host-mount. Missing/unreadable/invalid assets fail clearly; no implicit
  download or runtime model mutation is allowed.
- Expose the server's OpenAI-compatible API directly:
  - `GET /v1/models`;
  - `POST /v1/chat/completions` with non-streaming JSON;
  - the same chat endpoint with streaming SSE;
  - `GET /health` wired to Pod readiness.
- Use the existing generic NodePort data plane; do not add model-aware routing.
  Test inference Pod Service/Endpoints lifecycle (empty without ready backends,
  reachable when ready). G70/G71/ST1-ST6 are generic evidence, not a substitute.
- Build two acceptance harnesses: a default CPU/fake-inventory/fake-OpenAI
  server path and an opt-in real llama.cpp + local GGUF + real GPU path.

### Gates

- **INF1 ordinary objects:** apply the Deployment and Service with `kubectl`;
  observe a ready Pod and reachable generic NodePort without an Agent or CRD.
- **INF2 readiness/API:** `/health` gates readiness; `/v1/models` and chat
  non-streaming return valid OpenAI-shaped responses.
- **INF3 streaming:** chat streaming emits an ordered SSE sequence and clean
  termination through NodePort; a non-stream request must not be treated as a
  stream.
- **INF4 lifecycle:** Recreate holds the new RS at zero until actual old runtime
  teardown and ledger release complete, correlated to old Pods/attempts; a stale
  ack cannot unblock. Inject stalled teardown/release and assert replacement stays
  zero with actionable status. Invalid Deployment strategy is rejected.
- **INF5 offline/fake CI:** prove the complete lifecycle using fake inventory
  and a fake OpenAI server with network downloads disabled/forbidden. Assert
  no model-aware Router configuration was generated.
- **INF6 real acceptance/closure:** recorded execution with preinstalled pinned
  llama-server, local read-only GGUF, and real GPU is mandatory for whole-T7.6 done;
  repeat health/models/chat/streaming and prove actual accelerator execution without
  network access. Fake green closes only labelled fake subscopes and leaves T7.6 open.
  Normal CI may clearly SKIP missing real prerequisites; hardware-required mode must
  FAIL. No such real evidence exists now.

Model distribution/GC, automatic model deployment, multi-model routing, and
multi-model traffic policy remain outside this stage.

## 7. Tenant isolation versus Agent action containment

One tenant per cluster with tenant-dedicated nodes is **necessary but not sufficient** for hostile multi-tenant support, and Namespaces are not tenant security boundaries. Support is contingent on audited/tested per-tenant isolation for: network and ingress/egress; storage/NFS/registry/workspace; API, NodePort, Router, and kubelet endpoints; and provider/broker endpoints plus credential handling.
Until the required controls are present and tested, hostile multi-tenant deployment is unsupported; topology alone is not a complete boundary. Ordinary inference and Agent execution use different profiles:

- **Q60/T4.15 is an ordinary-inference performance profile, not Agent action
  containment and not a cross-tenant boundary.** Literal `/` exposes the host
  filesystem, host `/dev` exposes devices, and Q52 shares the host network
  namespace. These are acceptable only inside a tenant-dedicated cluster;
  nodes must not be shared across tenants.
- NEWNS/NEWPID/NEWUTS/NEWIPC/NEWCGROUP, read-only host-mounts, and CPU/memory
  cgroups retain their process/resource semantics but do not turn host root,
  devices, and a shared kernel into Agent or cross-tenant containment.
- `CUDA_VISIBLE_DEVICES` and `HIP_VISIBLE_DEVICES` select assigned devices.
  Whole-GPU allocation is logical exclusivity, not MIG, hardware partitioning,
  or protection from a malicious host-root process.
- **Q63/T7.7 is the Agent action-containment profile.** Its central
  default-deny gate intercepts forbidden tool/subagent actions, while private
  non-host rootfs, fresh netns, least privilege, mandatory cgroup limits, and
  descendant cleanup enforce the result in Linux. Sandboxed Agents never use
  Q60 literal host `/`, host netns, or host `/dev`.
- Tenant topology and Q63 action containment are complementary but do not replace
  the per-tenant controls above. NodePort exposure requires operator-controlled
  network access; TLS, authentication/RBAC, and network policy remain tracked work.
- Unavailable host-direct/GPU capability fails the ordinary inference Pod
  explicitly. Unavailable Agent sandbox/broker capability leaves the Agent
  Pending/Failed with an actionable reason. Neither path may silently change
  its security or device semantics through a fallback.

## 8. CPU/fake CI versus real-hardware acceptance

| Mode | Required inputs | Expected behavior | What it proves |
|------|-----------------|-------------------|----------------|
| Default unit/CPU CI | No GPU; privileged subset for nsrun where needed | Run unit/fake tests; clearly SKIP real-hardware e2e when prerequisites are absent | Selection, namespaces, accounting, ownership, API/lifecycle behavior |
| Fake integration | Fake inventory + fake OpenAI server; no network download | Must run and pass, never masquerade as hardware validation | Scheduler/allocator and Deployment/NodePort/OpenAI contract |
| Agent sandbox | Unit/fake policy tests, fake local provider, and privileged Linux integration | Run AS1–AS6 with no external provider/service; never substitute host-direct or containerd | Action policy, credential confinement, and Linux containment |
| Opt-in real hardware | Actual NVIDIA/AMD GPU + driver, pinned llama-server, operator-prestaged local GGUF | Run real GPU and inference checks; no downloads | Physical inventory, direct device execution, real llama.cpp behavior |
| Hardware-required | Same real inputs, explicitly required by the operator/CI job | Missing hardware/assets or any SKIP is a hard FAIL | Release/operator acceptance where real evidence is mandatory |

Every SKIP must name the missing prerequisite; real e2e must not download drivers, llama.cpp, or GGUF. Fake green closes only explicitly labelled fake subscopes. Recorded real evidence is mandatory for whole-TODO done: every vendor claimed by T7.2 (both if NVIDIA+AMD are claimed, otherwise narrow Q59 support), and T7.6's pinned llama-server + local read-only GGUF + real GPU health/models/chat/streaming run with proof of actual accelerator execution.
No such evidence exists now.

Each implementation stage also runs the repository's standard build/test/
clippy/safety gates and T0.6. Completion evidence belongs in the corresponding
TODO card and `index.md` at implementation time; no status is advanced by this
plan alone.

## 9. Deferred and still-tracked platform work

The P0 Agent sandbox and P1–P3 host-AI track take sequencing priority; they do
not delete or silently complete unrelated platform debt.

| Area | Still tracked as |
|------|------------------|
| Router TLS/dynamic certificate/SNI remainder | T5.5 not-started + T5.6 in-progress remainder |
| Authentication, TLS identity, authorization/RBAC | T1.3 not-started; bootstrap dependency T3.3 remains |
| HA multi-server | T3.4 not-started, with its existing dependencies |
| Network policy and flannel host-gw/multi-node remainder | T4.4 not-started + T4.3 in-progress remainder |
| PVC/hostPath consumption and local storage prerequisites | T4.10 not-started + T6.2 not-started |

Additional deliberate deferrals:

- Q59: dynamic CRD registration, admission, aggregation, protobuf, full RBAC,
  and full-conformance expansion unless a target ecosystem workload requires a
  specific supported addition.
- Q61: external/containerd device-plugins, fractional GPU, MIG, time-slicing,
  Agent-specific GPU integration, and broader scheduling policy work.
- Q62: model distribution and garbage collection, multi-model routing,
  automatic model Deployment controllers, and model-aware Router behavior.

These items remain visible in their existing TODOs/ADRs even when they are not
on the Phase 3 P0–P3 critical paths.
