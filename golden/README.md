# Golden Conformance Baseline (T0.6)

An immutable set of k3s/k8s wire-level behaviors that every later TODO must
keep green (Q2 merge gate). The harness `scripts/golden-conformance.sh` boots a
real `init-pro server` and diffs its responses against these fixtures; per-layer
suites live in `scripts/golden/suites/` (`storage.sh`, `controllers.sh`,
`scheduler.sh`, `nodes.sh`, `networking.sh`, `argoproj.sh`).

## Volatility

The only non-deterministic field is `APIVersions.serverAddress` (the bind
`host:port`); the harness normalizes it to the token `@@PORT@@` before diffing.
Everything else is byte-stable (sorted maps/sets, fixed verb lists, no
timestamps/uids/resourceVersions in discovery payloads).

## Cases

Current inventory: 100 case ids spanning G01-G113 across the 15 shard
scripts under `scripts/golden/suites/` (multi-leg cases G25/G26/G30 emit one
verdict per leg; the last full run on the dev host closed 75 PASS + 6 SKIP
under the foreign-cni0 posture below). The registry of shards and case ids is
machine-checked by `scripts/e2e-coverage.sh` (all-gates gate 9) against
`e2e/cases.d/*.toml`. SKIP policy (Q25): a missing build prerequisite (vendored
containerd bundle, `cc`) SKIPs the whole case rather than failing it — a
build-configuration state is not a conformance break.

| ID  | Suite | Asserts | Kept green by |
|-----|-------|---------|---------------|
| G01 | discovery | `GET /api` → APIVersions lists core `v1` | T0.6, T1.1 |
| G02 | discovery | `GET /apis` → APIGroupList includes init-pro.io | T0.6, T1.1 |
| G03 | discovery | `GET /api/v1` → core/v1 resource list (9 kinds, incl. PVCs) | T0.6, T1.1 |
| G04 | discovery | `GET /apis/init-pro.io/v1` → resource list (agents ag, builds bld, luarouters, servicepipelines sp, trafficsplits ts) | T0.6, T1.1 |
| G05 | discovery | unknown group/version → 404 | T0.6 |
| G06 | storage | empty pods collection list → 200 | T1.2b |
| G07 | storage | POST ConfigMap → 201 | T1.2b |
| G08 | storage | GET ConfigMap → 200 | T1.2b |
| G09 | storage | LIST ConfigMaps (1 item) → 200 | T1.2b |
| G10 | storage | DELETE ConfigMap → 200 | T1.2b |
| G11 | storage | deleted ConfigMap → 404 | T1.2b |
| G12 | storage | `?watch=1` stream opens → 200 | T1.2b |
| G13 | storage | SSA `apply-patch+yaml` creates → 201 | T1.2c |
| G14 | storage | SSA re-apply updates → 200 | T1.2c |
| G15 | storage | `?watch=1&resourceVersion=0` replays retained ADDED history | T2.2 |
| G16 | controllers | `GET /apis/apps/v1` byte-stable (deployments/rs/sts/ds + controllerrevisions) | T3.1a/T3.1b |
| G17 | controllers | Deployment scale 3→1 converges; Endpoints membership follows | T3.1a |
| G18 | controllers | rolling update converge + `kubectl rollout status` exit codes | T3.1b |
| G19 | controllers | StatefulSet ordinal identity scale 3→1 + per-ordinal PVCs retained | T3.1b |
| G20 | controllers | DaemonSet one-pod-per-matching-node across node lifecycle | T3.1b |
| G21 | controllers | GC cascade deployment→rs→pods + namespace drain/finalize | T3.1b |
| G22 | scheduler | nodeSelector placement + PodScheduled=True + Unschedulable | T3.2 |
| G23 | scheduler | HTTP extender filter steers placement via scheduler config | T3.2 |
| G24 | nodes | agent supervises containerd; `crictl version/ps` over CRI. SKIP: no vendor bundle; the registry pull smoke SKIPs best-effort | T4.1 |
| G25 | nodes | Deployment pod Running+Ready on the agent node over real containerd; container kill → kubelet restart; delete → zero sandboxes (legs a/b/c). SKIP: no vendor bundle, no cc (Q27 pause image), or a foreign host cni0 (coexistence, S40c) | T4.2 |
| G26 | nodes | readiness gates Ready, liveness restarts after /fail, emptyDir bootcount, configMap projection + hostname==pod (legs a-d). SKIP: no vendor bundle, no cc, or a foreign host cni0 (S40c) | T4.2 |
| G30 | nodes | kubectl logs relays container lines, `logs -f` follows appended lines, exec runs `/bin/wfstep` (rc0 + merged output) and propagates remote exit 1 (legs a-d). SKIP: no vendor bundle, no cc, or a foreign host cni0 (S40c) | T4.2 Scope C, Q31 |
| G31 | networking | `GET /apis/networking.k8s.io/v1` byte-stable (ingresses `ing`, ingressclasses) | Q32 router swap |
| G27 | argoproj | `GET /apis/argoproj.io/v1alpha1` byte-stable (wf/cwf/cronwf shortNames) | T7.3 Scope A |
| G28 | argoproj | Workflow + CronWorkflow CRUD/SSA lifecycle + generateName minting | T7.3 Scope A |
| G29 | argoproj | two-step Workflow runs to Succeeded over the agent/containerd stack; status reports Steps root + Succeeded Pod nodes (wfstep one-shot image, Q27). SKIP: no vendor bundle, no cc, or a foreign host cni0 (S40c) | T7.3 Scope B |

| G90 | phase3 | invalid explicit imagePullPolicy (Pod and apps/v1 templates) rejected 422 on create; template PUT replace + SSA apply also rejected (uniform admission seam) | T4.15, Q60 |
| G91 | phase3 | whole-GPU quantities must be non-negative integers: `0.5` and `"250m"` (nvidia.com/gpu) rejected 422 | T7.2, Q61 |
| G92 | phase3 | GPU request > limit per container rejected 422 with both values in the message | T7.2, Q61 |
| G93 | phase3 | GPU-requesting template without the literal `init-pro.io/rootfs: "/"` annotation rejected (incl. non-literal `"host"`), 422 Required value | T7.2/Q60 |
| G94 | phase3 | host-direct template (`init-pro.io/rootfs: /`) + RollingUpdate (default and explicit) rejected 422 — Recreate required | T4.15, Q60 |
| G95 | phase3 | positive: `/` + Recreate + `nvidia.com/gpu: 1` Deployment created 201; RS-materialized pod carries the template annotations verbatim (S47 annotation-propagation fix) | T4.15/T3.1 |
| G96 | phase3 | bad imagePullPolicy through Deployment template POST, pod PUT replace, and pod SSA apply -> 422 all three (uniform admission seam) | T4.15, Q60 |

## Growing the suite

This baseline started as the empty-cluster discovery-only contract (G01-G06)
and now spans CRUD/watch/SSA over the embedded store (T1.2b/T1.2c), watch
history replay (T2.2), the in-process controller manager (T3.1a/T3.1b), the
scheduler (T3.2), the node layer (T4.1 containerd supervision, T4.2 kubelet
end-to-end incl. probes/volumes/hostname/exec/logs), the networking.k8s.io
group (Q32 Ingress discovery), the argoproj.io group (T7.3
Scope A discovery/CRUD, Scope B a two-step Workflow reaching Succeeded on real
step pods), and the phase3 admission plane (S47: pull-policy/GPU-quantity/
host-direct-strategy validation + template-annotation propagation). When a layer adds a wire-visible behavior, append its golden case
here and to the matching suite file, then commit the new fixture. Flaky cases
are quarantined, never deleted.
