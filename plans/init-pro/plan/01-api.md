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

- **状态 / Status** — done (T1.2a discovery, T1.2b CRUD+watch, T1.2c server-side apply all complete)
- **证据 / Evidence** — Sprint 10: T1.2a discovery handlers landed (T0.6 golden 4/4). Sprint 10.5: T1.2b REST CRUD + watch landed in `crates/apiserver/` — `api_app()` builds the full router (discovery + CRUD + watch routes) over `Arc<dyn StorageBackend>`; `AppState` resolves resource paths, `collection.rs` (create/list/watch), `item.rs` (get/replace/delete/patch). Handlers: resourceVersion CAS on PUT/DELETE (409 on stale), strategic-merge/merge/JSON PATCH, namespace auto-injection, key-cursor list pagination, live-watch chunked stream (ADDED/MODIFIED/DELETED). 15 apiserver integration tests + 1 watch streaming test (real TCP). `cli/runtime.rs` constructs `EmbeddedStorage` and passes it to `serve()`. Server-side apply field-manager = T1.2c (next). Sprint 11: T1.2c server-side apply landed — `crates/api/src/apply/` (`field_set.rs` FieldTree/FieldsV1 extraction keyed by merge-key; `mod.rs` `apply_object` merge/conflict/prune + `get_managed_fields`/`set_managed_fields` round-trip) + `crates/apiserver/src/apply.rs` (PUT/PATCH dispatch on `application/apply-patch+yaml`: 201 create / 200 update (CAS) / 409 conflict / `force` ownership steal). +15 tests (8 api + 7 apiserver); golden G13/G14 → 14/14. T1.2 → done.
- **卡点 / Blockers** — Server-side apply field-manager complexity; opt-in
    scope for v1.
- **依赖 / Depends on** — T1.1, T2.2

---

## T1.3 — 认证授权 (kubeconfig/token/RBAC) (T1.3a TLS+静态 token + T1.3b RBAC)

- **目标 / Goal**
  Authn/Authz compatible with kubeconfig, bootstrap tokens, bearer tokens,
  and RBAC (`Role`/`RoleBinding`/`ClusterRole*`).

- **核心实现 / Core implementation**
  - kubeconfig parsing (k3s `clientaccess`, `link_repos/k3s/pkg/clientaccess/`).
  - x509 + bootstrap-token authn; anonymous off by default.
  - RBAC authorizer with the upstream attribute builder
    (`verb/resource/group/namespace/name`) and `system:` built-in roles.
  - webhook authn/authz seam (no-op default).
  - **T1.3a — TLS serving + 静态 token 认证 (S46 排期, Q47 草案, 修订
    Q11)**: server 自签 CA + serving cert（`--tls-san`）；axum `app.rs`
    插 authn 中间件（Bearer 静态 token，`--token-file` 从 noop 转
    wired）；kubectl 侧补 https 传输（现为裸 TcpStream，`http.rs` 直接
    拒 https；TLS 客户端选型 rustls vs 复用 openrusty 由 Q47 定夺）+
    kubeconfig 读取（server/CA/token 三字段起步）。golden 保持 h2c 明文
    基线不动（TLS 是 opt-in 旗标，避免全量重基线）；TLS 面由新
    `scripts/auth-e2e.sh` A1–A6 独立闭环（自签链、token 拒绝 401、
    kubeconfig 连通）。
  - **T1.3b — RBAC 授权 (S47 排期)**: Role/RoleBinding/ClusterRole/
    ClusterRoleBinding 注册 + authz 中间件（verbs×resources 匹配）；
    ServiceAccount 最小面（default SA + token）。

- **验收手段 / Acceptance**
  - Golden: a `kube-rs` client with a service-account token can/cannot
    perform operations per a test ClusterRole.
  - `kubectl auth can-i` parity cases.
  - T1.3a/b: 新 `scripts/auth-e2e.sh` —— A1–A6 (TLS/token) + A7–A10
    (RBAC allow/deny 矩阵)。

- **状态 / Status** — not-started
- **证据 / Evidence** — —
- **卡点 / Blockers** — none（TLS 客户端选型 rustls vs openrusty 复用
  留给 Q47 定稿）
- **依赖 / Depends on** — T1.1
