# Layer 1 — API

Mirrors `index.md` TODO IDs **T1.1–T1.3**. Q2 (full protocol conformance) is
the governing constraint: every resource, serialization, and auth path must
be wire-compatible with upstream kubectl/kube-rs.

---

## T1.1 — 资源模型与 API group schema

- **目标 / Goal**
  A Rust resource model faithful to Kubernetes API machinery: `TypeMeta`,
  `ObjectMeta`, `ListMeta`, GVK↔GVR, and byte-faithful JSON/protobuf +
  StrategicMergePatch semantics.

- **核心实现 / Core implementation**
  - Built on `kube-core` 4.x + `k8s-openapi` 0.28 (`v1_32` feature) core
    types; add our own schema registry for init-pro API groups
    (`init-pro.io/*`) and pass-through for native groups.
  - **Wire format = JSON-only for v1 (decision Q10):** API server, etcd
    storage, and watch all use `application/json`. protobuf is explicitly
    deferred — see `decisions.md` Q10.
  - `ApiVersion` parsing + GVK↔GVR helpers; `SchemaRegistry` mapping GVK →
    type info (kind/resource/plural/list-kind/scope) for core/v1 types
    (Pod, ConfigMap, Secret, Service, Namespace, Node, Event).
  - Byte-faithful JSON round-trip (serde) on a fixed object set; StrategicMergePatch
    core algorithm (containers/volumes/labels by merge-key) + JSONPatch fallback.
  - OpenAPI/v2 schema discovery skeleton (served by T1.2; full `kubectl explain`
    is gated behind a running server — see Q10 consequences).

- **验收手段 / Acceptance**
  - `cargo test` JSON round-trip equality (deserialize → serialize → bytes
    equal canonical fixtures) on Pod/ConfigMap/Namespace representative objects.
  - GVK→GVR conversion round-trips without information loss.
  - StrategicMergePatch: container list merges by name, volumes merge by name.
  - **Deferred to T0.6/T1.2:** `kubectl get --raw` + `kube-rs` round-trip
    (needs the HTTP server); `kubectl explain pods.spec` (needs discovery +
    running server — recorded in Q10).

- **状态 / Status** — done
- **证据 / Evidence** — `crates/api`: GVK/GVR wrappers (`gvk.rs`),
    schema registry (`schema.rs`), JSON round-trip (`serde_ext.rs` +
    `tests/json_fidelity.rs`), StrategicMergePatch (`patch.rs`), `init-pro.io` CRD types
    (`initpro.rs`), discovery doc builders (`discovery.rs`). 39 tests in
    `api` + 1 in `cli`; `cargo test --locked --workspace`
    181 green; `cargo clippy --workspace --all-targets -- -D warnings` clean.
- **卡点 / Blockers** — Resolved by **Q10** (JSON-only v1; protobuf deferred).
- **依赖 / Depends on** — T0.3

---

## T1.2 — APIServer 核心 (REST + kubectl 真实交互)

- **目标 / Goal**
  A working API server: etcd-backed REST handlers for core resources,
  supporting `kubectl get/apply/watch` and `kube-rs` clients (Q2
  protocol-level acceptance).

- **核心实现 / Core implementation**
  - `axum`/`hyper` front-end exposing `/api`, `/apis/*`, `/api/v1/...`.
  - Generic REST storage: each resource maps to etcd keys
    (`/registry/<group>/<version>/<resource>/<ns>/<name>` — upstream layout).
  - Watch: etcd watch → filter/project → chunked `application/json` stream
    with `resourceVersion` monotonicity.
  - `apply` (server-side): field manager + StrategicMergePatch
    (hardest semantic; reuse upstream patch library semantics).
  - Admission webhook seam (no-op default for v1).

- **验收手段 / Acceptance**
  - End-to-end script: `kubectl apply -f pod.yaml`; `kubectl get pod -oyaml`;
    `kubectl get pod -w` (assert events stream); `kubectl delete`.
  - kube-rs example binary does the same against `init-pro`.
  - All cases pinned in T0.6 golden.

- **状态 / Status** — in-progress (T1.2a discovery done; T1.2b CRUD+watch done; T1.2c server-side apply deferred)
- **证据 / Evidence** — Sprint 10: T1.2a discovery handlers landed (T0.6 golden 4/4). Sprint 10.5: T1.2b REST CRUD + watch landed in `crates/apiserver/` — `api_app()` builds the full router (discovery + CRUD + watch routes) over `Arc<dyn StorageBackend>`; `AppState` resolves resource paths, `collection.rs` (create/list/watch), `item.rs` (get/replace/delete/patch). Handlers: resourceVersion CAS on PUT/DELETE (409 on stale), strategic-merge/merge/JSON PATCH, namespace auto-injection, key-cursor list pagination, live-watch chunked stream (ADDED/MODIFIED/DELETED). 15 apiserver integration tests + 1 watch streaming test (real TCP). `cli/runtime.rs` constructs `EmbeddedStorage` and passes it to `serve()`. Server-side apply field-manager = T1.2c (next).
- **卡点 / Blockers** — Server-side apply field-manager complexity; opt-in
    scope for v1.
- **依赖 / Depends on** — T1.1, T2.2

---

## T1.3 — 认证授权 (kubeconfig/token/RBAC)

- **目标 / Goal**
  Authn/Authz compatible with kubeconfig, bootstrap tokens, bearer tokens,
  and RBAC (`Role`/`RoleBinding`/`ClusterRole*`).

- **核心实现 / Core implementation**
  - kubeconfig parsing (k3s `clientaccess`, `link_repos/k3s/pkg/clientaccess/`).
  - x509 + bootstrap-token authn; anonymous off by default.
  - RBAC authorizer with the upstream attribute builder
    (`verb/resource/group/namespace/name`) and `system:` built-in roles.
  - webhook authn/authz seam (no-op default).

- **验收手段 / Acceptance**
  - Golden: a `kube-rs` client with a service-account token can/cannot
    perform operations per a test ClusterRole.
  - `kubectl auth can-i` parity cases.

- **状态 / Status** — not-started
- **证据 / Evidence** — —
- **卡点 / Blockers** — none
- **依赖 / Depends on** — T1.1
