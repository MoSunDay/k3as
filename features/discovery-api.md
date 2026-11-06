# discovery-api

Byte-correct Kubernetes API discovery served over HTTP from an in-memory
`SchemaRegistry`.

## What it is

The discovery surface only - what `kubectl api-resources` / client-go reads
to learn the cluster's API surface. Bodies are built by pure functions and
serialized via `axum::Json` (`Content-Type: application/json`, the sole v1
wire format, Q10). CRUD/watch/SSA are NOT here - they live in the
apiserver REST face (T1.2b/T1.2c, `crates/apiserver/src/lib.rs`).

## Endpoints

| Method + path | Returns |
|---------------|---------|
| `GET /api` | `APIVersions` (core group) |
| `GET /apis` | `APIGroupList` (non-core groups) |
| `GET /api/v1` | `APIResourceList` (core/v1 index) |
| `GET /apis/:group/:version` | `APIResourceList` for a known group/version |
| `GET /apis/:group/:version` | 404 for an unknown group/version |

## Where it lives

- `crates/api/src/discovery.rs` - pure builders: `core_api_versions()`,
  `api_group_list()`, `api_resource_list()`. Unit-tested for byte fidelity
  against upstream `meta/v1` (`crates/api/tests/json_fidelity.rs`).
- `crates/apiserver/` - thin axum transport: `discovery_handlers.rs` wires
  the routes; `serve.rs` boots the server on a free loopback port.
- `golden/` - byte-stable fixtures + `README.md`, the golden-suite SSOT
  (inventory G01-G31, 39 PASS sites); cards do not self-maintain counts.
- `scripts/golden-conformance.sh`, `scripts/apiserver-discovery-parity-test.sh`.

## Status / scope

- Done as T1.1 (resource model + `SchemaRegistry`) + T1.2a (HTTP
  transport). T1.2 closed done in full (a/b/c): discovery (T1.2a) + REST
  CRUD/watch over the storage trait (T1.2b, `crates/apiserver/src/lib.rs`)
  + Server-Side Apply (T1.2c, `crates/api/src/apply/` +
  `crates/apiserver/src/apply.rs`; card: server-side-apply.md).
- Discovery responses still come from the static `SchemaRegistry`; stored
  resources are served by the CRUD face, e.g. pods collection list -> 200
  (G06, T1.2b).
