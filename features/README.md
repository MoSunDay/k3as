# Feature index

Concise cards, not specs. Detailed specs live in `plans/init-pro/plan/*.md`;
status of record lives in `plans/init-pro/index.md`.

| Feature | Status | Card | Owner layer | Key files |
|---------|--------|------|-------------|-----------|
| discovery-api | done (T1.1 + T1.2a) | [card](discovery-api.md) | api, apiserver | `crates/api/src/discovery.rs`, `crates/apiserver/`, `golden/` |
| router-data-plane | done / M1 (T5.1-T5.4) | [card](router-data-plane.md) | router | `crates/router/` |
| storage-layer | landed (T2.1/T2.2/T2.3) | [card](storage-layer.md) | storage | `crates/storage/` (`backend.rs`, `embedded.rs`, `sqlite/`) |
| controllers | done (T3.1a+T3.1b) | [card](controllers.md) | controllers | `crates/controllers/`, `crates/cli/src/runtime.rs` |
| scheduler | done (T3.2) | [card](scheduler.md) | scheduler | `crates/scheduler/`, `crates/apiserver/src/binding.rs` |
| containerd-runtime | done (T4.1) | [card](containerd-runtime.md) | runtime, multicall | `crates/runtime/`, `crates/multicall/src/lib.rs`, `vendor/versions.toml` |
| kubelet | Scope A done (T4.2) | [card](kubelet.md) | kubelet, apiserver, runtime | `crates/kubelet/`, `crates/apiserver/src/pod_status.rs`, `scripts/build-pause-image.sh` |
| multicall-cli | done (T0.1, T0.4) | - (see SSOT) | multicall, cli | `crates/multicall/`, `crates/cli/` |
| build-bundling | done (T0.2) | - (see SSOT) | vendor | `crates/vendor/`, `crates/init-pro/build.rs` |

Notes:
- Cards summarize what an agent needs to orient quickly; they point back to
  the SSOT for detail. multicall-cli and build-bundling have no dedicated
  card yet - read their T0.1/T0.4/T0.2 entries in `plan/00-foundation.md`.
- The kubelet card tracks T4.2: Scope A (pod lifecycle + status over the
  CRI seam) is done; Scope B/C (probes, volumes, local deploy) remains.
- Sprint-level history: `CHANGELOG.md`. Feature-level deltas: [changelog.md](changelog.md).
