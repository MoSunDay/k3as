# Golden Conformance Baseline (T0.6)

An immutable set of k3s/k8s wire-level behaviors that every later TODO must
keep green (Q2 merge gate). The harness `scripts/golden-conformance.sh` boots a
real `init-pro server` and diffs its responses against these fixtures.

## Volatility

The only non-deterministic field is `APIVersions.serverAddress` (the bind
`host:port`); the harness normalizes it to the token `@@PORT@@` before diffing.
Everything else is byte-stable (sorted maps/sets, fixed verb lists, no
timestamps/uids/resourceVersions in discovery payloads).

## Cases

| ID  | Endpoint                                                      | Fixture                     | Asserts                                                  | Kept green by |
|-----|---------------------------------------------------------------|-----------------------------|----------------------------------------------------------|---------------|
| G01 | `GET /api`                                                    | `discovery-api.json`        | APIVersions lists core `v1`                              | T0.6, T1.1    |
| G02 | `GET /apis`                                                   | `discovery-apis.json`       | APIGroupList includes init-pro.io                        | T0.6, T1.1    |
| G03 | `GET /api/v1`                                                 | `discovery-core-v1.json`    | core/v1 APIResourceList (7 kinds)                        | T0.6, T1.1    |
| G04 | `GET /apis/init-pro.io/v1`                                    | `discovery-initpro-v1.json` | luarouters CRD resource list                             | T0.6, T1.1    |
| G05 | `GET /apis/fabricated.io/v9beta1`                             | (status 404)                | unknown group/version → 404                              | T0.6          |
| G06 | `GET /api/v1/pods`                                            | (status 200)                | empty pods collection list                               | T1.2b         |
| G07 | `POST /api/v1/namespaces/default/configmaps`                  | (status 201)                | create ConfigMap golden-cm → 201                         | T1.2b         |
| G08 | `GET /api/v1/namespaces/default/configmaps/golden-cm`         | (status 200)                | get ConfigMap golden-cm → 200                            | T1.2b         |
| G09 | `GET /api/v1/namespaces/default/configmaps`                   | (status 200)                | list ConfigMaps (1 item) → 200                           | T1.2b         |
| G10 | `DELETE /api/v1/namespaces/default/configmaps/golden-cm`      | (status 200)                | delete ConfigMap golden-cm → 200                         | T1.2b         |
| G11 | `GET /api/v1/namespaces/default/configmaps/golden-cm`         | (status 404)                | deleted ConfigMap is gone → 404                          | T1.2b         |
| G12 | `GET /api/v1/namespaces/default/configmaps?watch=1`           | (status 200)                | watch stream opens → 200                                 | T1.2b         |
| G13 | `PATCH /api/v1/namespaces/default/configmaps/golden-apply-cm` | `apply-patch+yaml`          | creates golden-apply-cm → 201 (fieldManager=golden-test) | T1.2c         |
| G14 | `PATCH /api/v1/namespaces/default/configmaps/golden-apply-cm` | `apply-patch+yaml`          | updates golden-apply-cm → 200 (fieldManager=golden-test) | T1.2c         |
| G15 | `GET /api/v1/namespaces/default/configmaps?watch=1&resourceVersion=0` | (poll grep)                | watch replays retained history (ADDED)                   | T2.2          |
| G16 | `GET /apis/apps/v1`                                           | `discovery-apps-v1.json`    | apps/v1 APIResourceList (deployments/replicasets/statefulsets/daemonsets) | T3.1a         |
| G17 | `POST/PUT .../deployments/golden-dep` + pods/endpoints polls  | (convergence poll)          | Deployment scale 3→1 converges; Endpoints reflect membership | T3.1a         |

## Growing the suite

This baseline now spans discovery (T1.2a) plus a CRUD/watch/server-side-apply
round-trip over the embedded store (T1.2b), watch history replay (T2.2), and
the apps/v1 discovery + Deployment-convergence acceptance of the in-process
controller manager (T3.1a); it started life as the
empty-cluster discovery-only contract (G01-G06). When a layer adds a
wire-visible behavior, append its golden case here and to
`scripts/golden-conformance.sh`, then commit the new fixture. Flaky cases are
quarantined, never deleted.
