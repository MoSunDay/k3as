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

| ID  | Endpoint                  | Fixture                      | Asserts                              | Kept green by |
|-----|---------------------------|------------------------------|--------------------------------------|---------------|
| G01 | `GET /api`                | `discovery-api.json`         | APIVersions lists core `v1`          | T0.6, T1.1    |
| G02 | `GET /apis`               | `discovery-apis.json`        | APIGroupList includes init-pro.io    | T0.6, T1.1    |
| G03 | `GET /api/v1`             | `discovery-core-v1.json`     | core/v1 APIResourceList (7 kinds)    | T0.6, T1.1    |
| G04 | `GET /apis/init-pro.io/v1`| `discovery-initpro-v1.json`  | luarouters CRD resource list         | T0.6, T1.1    |
| G05 | `GET /apis/fabricated.io/v9beta1` | (status 404)         | unknown group/version → 404          | T0.6          |
| G06 | `GET /api/v1/pods`        | (status 404)                 | no collection endpoint yet           | T0.6 → T1.2   |

## Growing the suite

This is the **empty-cluster baseline**: the server is discovery-only (T1.2a).
When a layer adds a wire-visible behavior, append its golden case here and to
`scripts/golden-conformance.sh`, then commit the new fixture. Flaky cases are
quarantined, never deleted.
