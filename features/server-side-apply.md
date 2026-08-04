# server-side-apply

Server-Side Apply (SSA) field-manager for the apiserver REST face (T1.2c).
NEW. Lives in `crates/api/src/apply/` + `crates/apiserver/src/apply.rs`.

## What it is

A k8s-faithful Server-Side Apply implementation: a client sends a desired
object with Content-Type `application/apply-patch+yaml` (and
`?fieldManager=<name>&force=<bool>`); the server records per-field ownership
in `metadata.managedFields`, merges/conflicts against other managers, and
prunes fields the same manager no longer declares. This is what makes
`kubectl apply` work end-to-end.

## Algorithm

- **Field extraction** (`field_set.rs`): walk a JSON object into a
  `FieldTree` (FieldsV1) — a recursive map of owned field paths. Container
  arrays are keyed by their merge key (e.g. container `name`) so list
  elements are owned individually, mirroring strategic-merge-patch semantics.
- **Apply** (`mod.rs::apply_object`): given live + desired, build the
  desired field set and diff against existing managers' sets:
  - same manager → merge (replace that manager's owned set).
  - a desired field already owned by *another* manager → emit a `Conflict`
    (path + owning manager); HTTP 409 unless `force=true`, which re-homes
    ownership to the applying manager.
  - fields the applying manager owned before but no longer declares →
    pruned (owning set updated).
  - unowned live fields are left untouched.
- **managedFields round-trip**: `get_managed_fields` / `set_managed_fields`
  (de)serialize the v1 `managedFields[].fieldsV1` blob.

## HTTP wiring (`crates/apiserver/src/apply.rs`)

- Dispatched from the PUT/PATCH item handlers when `Content-Type` contains
  `apply-patch`.
- `do_apply`: resolve resource + scope, read the live entry, run
  `apply_object`, return 409 on conflicts, else `create` (201) or `update`
  with resourceVersion CAS (200), stamping `metadata.managedFields`.
- JSON bodies only this sprint (the `+yaml` suffix is for wire-compat; YAML
  parsing is deferred).

## Status / next

- Landed: core algorithm + 15 tests (8 `crates/api/tests/apply.rs`,
  7 `crates/apiserver/tests/rest_apply.rs`); golden G13/G14 → 14/14.
- SSOT: T1.2c deferred → done; T1.2 → done (a/b/c complete).
- Limitations: no `time` stamp on managedFields (RFC-3339 deferred); no
  client-side `last-applied` annotation; conflict causes are reported as
  (path, manager) pairs rather than the full v1 Status `causes` shape.
