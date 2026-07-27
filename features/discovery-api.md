# discovery-api

Byte-correct Kubernetes API discovery served over HTTP from an in-memory
`SchemaRegistry`.

## What it is

The discovery surface only - what `kubectl api-resources` / client-go reads
to learn the cluster's API surface. Bodies are built by pure functions and
serialized via `axum::Json` (`Content-Type: application/json`, the sole v1
wire format, Q10). No CRUD / watch / persistence.

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
- `golden/` - 4 byte-stable fixtures + `README.md` (cases G01-G06).
- `scripts/golden-conformance.sh`, `scripts/apiserver-discovery-parity-test.sh`.

## Status / scope

- Done as T1.1 (resource model + `SchemaRegistry`) + T1.2a (HTTP
  transport). The server boots and serves discovery.
- NO CRUD / watch / persistence - that is T1.2, which was blocked on
  storage (T2.2, now landed). The server is a discovery-only shell today.
- Discovery responses come from the static `SchemaRegistry`; no resources
  are stored, so collection endpoints beyond discovery 404 (G05/G06).
