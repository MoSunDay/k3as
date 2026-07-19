# Layer 7 — AI Agent & Workflow scheduling (Phase 2)

Mirrors `index.md` TODO IDs **T7.1–T7.3**. Per **Q3**, this layer is gated
behind M5 (Layers 0–6 done). It extends the *generic* cluster with
AI-agent-aware scheduling via the standard scheduler-extender seam (T3.2),
**and** with argo-workflows integrated as a first-class built-in so
`Workflow`/`CronWorkflow` are created and scheduled natively by init-pro
(T7.3).

---

## T7.1 — AI Agent workload CRD + scheduler extender

- **目标 / Goal**
  A `init-pro.io/v1alpha1` `AgentWorkload` CRD and an out-of-process scheduler
  extender (upstream-compatible) that places agent pods by agent-specific
  semantics (affinity to peers, context locality, model availability).

- **核心实现 / Core implementation**
  - CRD with spec for agent group, model/image, context refs, SLO hints.
  - Extender implements the upstream HTTP filter/score protocol; hooks
    into T3.2's extender seam.
  - Controller (T3.1 pattern) materializes `AgentWorkload` → Pod(s).

- **验收手段 / Acceptance**
  - Golden: schedule a co-located agent group; extender scored nodes per
    policy; status reflects agent-specific conditions.

- **状态 / Status** — not-started
- **证据 / Evidence** — —
- **卡点 / Blockers** — CRD API design not finalized; defer until M5.
- **依赖 / Depends on** — T3.2, T1.2

---

## T7.2 — GPU/资源调度与策略

- **目标 / Goal**
  GPU and accelerator-aware scheduling (nvidia/amd device plugins +
    fractional GPU / time-slicing policies) for agent workloads.

- **核心实现 / Core implementation**
  - Device-plugin contract (gRPC) to advertise GPUs as extended resources.
  - Scheduler scoring (T3.2/T7.1) honoring GPU type, memory, time-slice.
  - Optional Router integration (T5.7) to steer ingress by model endpoint.

- **验收手段 / Acceptance**
  - Golden (best-effort, requires hardware): a GPU-flagged agent pod lands
    on the GPU node; time-slicing limit enforced.

- **状态 / Status** — not-started
- **证据 / Evidence** — —
- **卡点 / Blockers** — Hardware-dependent acceptance; provide a mock
    device plugin for CI.
- **依赖 / Depends on** — T7.1, T4.2

---

## T7.3 — argo-workflows 最小化整合 (Workflow/CronWorkflow 一级公民内置)

- **目标 / Goal**
  Minimal, first-class integration of argo-workflows: `Workflow` and
  `CronWorkflow` are **built-in** resources that init-pro creates and
  schedules directly — no separate addon/controller deployment. They are
  first-class citizens alongside Pods/Deployments, callable from the same
  API/CLI surface.

- **核心实现 / Core implementation**
  - **PLACEHOLDER — `link_repos/argo-workflows` memory still being generated;
    refine this section once upstream analysis lands.**
  - Expose `Workflow` / `CronWorkflow` as native API objects (CRD baked into
    init-pro API groups, or promoted to built-in types) so `kubectl get wf`
    works with zero install.
  - Embed the argo workflow controller + cron controller into the single
    `init-pro` binary (Q1 multicall; supervisor via T0.3), running
    server-side like other Layer-3 control loops.
  - Reuse init-pro scheduler (T3.2) and kubelet (T4.2) for pod execution —
    argo steps land as pods; no parallel executor.
  - Minimal CRD subset: steps/dag templates, artifacts (object-store backed),
    retries, cron schedules; defer plugins/UI to a later TODO.
  - RBAC + namespace isolation (T1.3); service-account token per workflow.

- **验收手段 / Acceptance**
  - Golden (T0.6): `kubectl apply -f workflow.yaml` (a 2-step DAG) → steps
    run to completion as pods; status reports node phases; artifacts retrievable.
  - `kubectl apply -f cronworkflow.yaml` → fires on schedule; concurrent-run
    policy honored.
  - No external install step: a fresh `init-pro` cluster lists `Workflow`
    via `kubectl api-resources` out of the box.

- **状态 / Status** — not-started
- **证据 / Evidence** — —
- **卡点 / Blockers** — **`link_repos/argo-workflows` not yet analyzed
    (memory being generated).** Upstream architecture, CRD versions, artifact
    storage choice, and the embed-vs-rewrite decision all pending. License
    review (Apache-2.0 expected) pending.
- **依赖 / Depends on** — T1.2, T3.1
