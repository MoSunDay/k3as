# anyhome-pod-mounts (T7.5, Q43)

Any Pod can mount anyhome NFS memory with two annotations; the
apiserver defaults them into real NFS volumes/mounts, and the anyhome
keeper pre-creates the server-side directories. `init-pro.io/anyhome`
= `rw`|`ro` mounts `/workspace`-style memory at the annotated path;
`init-pro.io/anyhome-mounts` = repeatable `path[=rw|ro]` entries for
extra paths. Decision: Q43; card: T7.5; detail in
`plans/init-pro/plan/07-agent-scheduling.md`.

## Shape

- `crates/common`: annotation parser (spelling + traversal-reject
  validation, dedup, volume-name determinism; 8 unit tests).
- `crates/apiserver/src/pod_workspace.rs`: `prepare(workspace_nfs,
  &mut body)` pure-JSON surgery, idempotent (volume name / nfs path /
  mount-name dedup). Injected uniformly at `do_create` + `do_apply`
  (every apply - SSA would strip a manager-owned field a later apply
  omits) + `do_replace` (36b: a stale-manifest PUT replace no longer
  strips injected volumes). Invalid annotation value -> 422 Invalid.
- `--disable-anyhome` (`AppState.workspace_nfs == None`) is fully
  lazy: no validation, byte-identical Pods.
- `crates/cli/src/anyhome_keeper.rs`: the keeper beat adds a third
  page - LIST pods, filter by annotation, `mkdir -p /pod/<ns>/<name>`
  and each custom server path (`anyhome.rs::prepare_fs_roots` also
  creates the `pod/` root). Deletion stays ignored (A5).
- kubelet: zero change - the generic `nfs` volume + `readOnly`
  (volume-level OR mount-level) from Sprint 20 does the work; `ro` is
  client-enforced (CRI readonly bind), same trust model as native k8s
  NFS.

## Name resolution (36b, the collision ladder)

Two container paths that sanitize identically (`/A_B` + `/a-b` ->
`anyhome-a-b`) once silently dropped the second volume while its mount
bound the FIRST volume's server path. `resolve_volume_name` now
resolves, in order: same-nfs-path alias (one volume, several mounts;
also binds the Workflow engine's `workspace` volume instead of
dangling), unclaimed base, A4-owned base (a user volume that already
carries the derived name wins - bind, never rename; incoming volume
names are snapshotted before any injection), else deterministic
`-2`/`-3` suffix in annotation order. `ensure_container_mount` dedups
per-mountPath only. The identity guard (rejecting `/`-bearing names)
fires only when an anyhome annotation is present.

## Evidence

- `cargo test --workspace --locked` 991 passed / 0 failed. T7.5
  suites: common parser 8; apiserver `pod_workspace` 10 + 5 (36b) unit
  and 8 + 2 (36b) wire (`tests/anyhome_pod_mounts.rs`: post-201 shape,
  ro readOnly, multi-segment, bare-Pod no-volumes key, plane-off
  laziness, SSA re-apply idempotent, A4 user wins, replace re-defaults
  a stripped body, on-the-wire collision); cli keeper pods page 3.
- `scripts/anyhome-e2e.sh` 7 passed / 0 failed (A1/P2/K1 + P3 rw
  defaulting + bare-Pod byte stability + keeper <=15 s dirs, P4 ro ->
  readOnly:true, P5 dual custom paths).
- `scripts/golden-conformance.sh` 73 passed / 0 failed, exit 0 -
  un-annotated Pods are byte-identical, so T0.6 is untouched.

## Limits / next steps

- No GC for `/pod/<ns>/<name>` dirs (A5 continuation; needs an owner
  model - frequent rebuilds accumulate directories).
- Keeper polls a full pod LIST per beat; fine at v1 scale, switch to
  watch later.
- `ro` is client-enforced; server-side per-path ro would need nfsserve
  vfs support (not planned).
- initContainers are deliberately not injected (Q43 post-review); no
  per-path authz in v1 - client-trust model like native k8s NFS.
