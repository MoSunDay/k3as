# Golden Conformance Baseline (T0.6)

An immutable set of k3s/k8s wire-level behaviors that every later TODO must
keep green (Q2 merge gate). The harness `scripts/golden-conformance.sh` boots a
real `init-pro server` and diffs its responses against these fixtures; per-layer
suites live in `scripts/golden/suites/` (`storage.sh`, `controllers.sh`,
`scheduler.sh`, `nodes.sh`, `argoproj.sh`).

## Volatility

The only non-deterministic field is `APIVersions.serverAddress` (the bind
`host:port`); the harness normalizes it to the token `@@PORT@@` before diffing.
Everything else is byte-stable (sorted maps/sets, fixed verb lists, no
timestamps/uids/resourceVersions in discovery payloads).

## Cases

Current inventory: G01-G29 (34 PASS sites; multi-leg cases G25/G26/G29 emit one
verdict per leg). SKIP policy (Q25): a missing build prerequisite (vendored
containerd bundle, `cc`) SKIPs the whole case rather than failing it — a
build-configuration state is not a conformance break.

| ID  | Suite | Asserts | Kept green by |
|-----|-------|---------|---------------|
| G01 | discovery | `GET /api` → APIVersions lists core `v1` | T0.6, T1.1 |
| G02 | discovery | `GET /apis` → APIGroupList includes init-pro.io | T0.6, T1.1 |
| G03 | discovery | `GET /api/v1` → core/v1 resource list (9 kinds, incl. PVCs) | T0.6, T1.1 |
| G04 | discovery | `GET /apis/init-pro.io/v1` → luarouters CRD resource list | T0.6, T1.1 |
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
| G25 | nodes | Deployment pod Running+Ready on the agent node over real containerd; container kill → kubelet restart; delete → zero sandboxes (legs a/b/c). SKIP: no vendor bundle or no cc (Q27 pause image) | T4.2 |
| G26 | nodes | readiness gates Ready, liveness restarts after /fail, emptyDir bootcount, configMap projection + hostname==pod (legs a-d). SKIP: no vendor bundle or no cc | T4.2 |
| G27 | argoproj | `GET /apis/argoproj.io/v1alpha1` byte-stable (wf/cwf/cronwf shortNames) | T7.3 Scope A |
| G28 | argoproj | Workflow + CronWorkflow CRUD/SSA lifecycle + generateName minting | T7.3 Scope A |
| G29 | argoproj | two-step Workflow runs to Succeeded over the agent/containerd stack; status reports Steps root + Succeeded Pod nodes (wfstep one-shot image, Q27). SKIP: no vendor bundle or no cc | T7.3 Scope B |

## Growing the suite

This baseline started as the empty-cluster discovery-only contract (G01-G06)
and now spans CRUD/watch/SSA over the embedded store (T1.2b/T1.2c), watch
history replay (T2.2), the in-process controller manager (T3.1a/T3.1b), the
scheduler (T3.2), the node layer (T4.1 containerd supervision, T4.2 kubelet
end-to-end incl. probes/volumes/hostname), and the argoproj.io group (T7.3
Scope A discovery/CRUD, Scope B a two-step Workflow reaching Succeeded on real
step pods). When a layer adds a wire-visible behavior, append its golden case
here and to the matching suite file, then commit the new fixture. Flaky cases
are quarantined, never deleted.
