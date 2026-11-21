# Layer 1 — API

Mirrors `index.md` TODO IDs **T1.1–T1.3**. Q59 (supported-subset protocol
compatibility) is the forward governing constraint; Q2's full-conformance
goal is historical. Every supported resource, serialization, and auth path
must be wire-compatible with upstream kubectl/kube-rs, without implying
complete Kubernetes API coverage.

---

## T1.1 — 资源模型与 API group schema

- **目标 / Goal**
  A Rust resource model faithful to Kubernetes API machinery: `TypeMeta`,
  `ObjectMeta`, `ListMeta`, GVK↔GVR, and byte-faithful JSON +
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
  A working API server: etcd-backed REST handlers for supported core resources,
  supporting `kubectl get/apply/watch` and `kube-rs` clients (Q59
  supported-subset, protocol-level acceptance).

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

## T1.3 — 认证授权 (T1.3a TLS+静态 token + T1.3b RBAC supported-subset, Q47/Q59; S50 落地)

- **目标 / Goal**
  Authn/Authz compatible with kubeconfig, bearer tokens, and RBAC
  (`Role`/`RoleBinding`/`ClusterRole*`) — delivered as a Q59
  supported-subset; 子集外项见下方留白清单与 phase-4 §6 台账。

- **核心实现 / Core implementation (as delivered, S50)**
  - **T1.3a — TLS serving + 静态 token 认证 (Q47 LOCKED)**: `rcgen
    0.13.2` 直依赖 (features crypto/pem/ring) 自签 CA + serving cert
    (SAN localhost/127.0.0.1 + `--tls-san` 追加), `tokio-rustls`
    acceptor 挂 axum (`apiserver::tls`); `--token-file`/
    `INIT_PRO_TOKEN_FILE` wired 为 kubeadm CSV
    (`token,user,uid,"groups"`，引号内逗号分隔组)，**fail-closed** —
    文件不可读/畸形 → server 拒绝起播 (rc=1, 日志
    `refusing to serve unauthenticated`)，绝不退化为无认证服务; CA 落盘
    `<data-dir>/server/tls/server-ca.crt`。serve.rs 重构为
    `ServeOptions` (h2c/TLS 双姿态)，就绪日志保 `discovery listening`
    子串 (golden/e2e 起播判据)。
  - **kubectl https + kubeconfig (Q47)**: 单 rustls 栈 (ring
    provider，无双栈)；**仅信任 kubeconfig `certificate-authority`**
    (无 CA 的 https 是硬错误 `kubeconfig server is https:// but
    carries no certificate-authority (Q47)`，不落系统 CA); kubeconfig
    server/CA/token 三字段起步 (完整 parity = Q47 显式弃置)，server
    优先级 flag > `INIT_PRO_SERVER` > kubeconfig > DEFAULT_SERVER。
  - **T1.3b — RBAC 授权 (Q59 supported-subset)**: `api::rbac` 四 wire
    类型 (Role/RoleBinding/ClusterRole/ClusterRoleBinding);
    `apiserver::authz` 授权中间件 (verb/resource/group/namespace/name
    属性匹配 + 非资源 URL 规则) + `authz_bootstrap`
    (cluster-admin@system:masters + system:node@system:nodes，kubelet
    token 开箱可用)；**discovery 条件注册** — RBAC 组与
    ServiceAccount 仅在 `--token-file` armed 时注册
    (`served_schema(auth_armed)`)，默认姿态 golden G02/G03 字节 pin
    不变 (G112 交叉断言)。
  - **P3 节点侧 token 透传**: agent `--token`/`INIT_PRO_TOKEN`
    (WiredShared，server 侧传 None) → run_agent/run_supervised →
    kubelet (`KubeletConfig::with_server_token`；
    `HttpJson::with_token` 于 `request_head_ct` 单一 chokepoint 注
    Authorization，request+watch 全覆盖) +
    `AgentRunnerConfig.server_token`; flanneld 不读 env token，走生成
    kubeconfig (`<data-dir>/agent/flannel/kubeconfig.yaml`，
    `user.token` 白名单校验)；flannel_keeper 带 token 起 keeper。
  - **Q59 子集外 (显式留白，phase-4 §6 standing)**: x509 客户端证书
    authn、bootstrap token 动态轮转、webhook authn/authz seam、
    anonymous 显式关断面、`kubectl auth can-i`。

- **验收手段 / Acceptance (as delivered)**
  - `scripts/auth-e2e.sh` **A1-A7** (真机): authn 401 (匿名/坏
    token) → RBAC allow/deny 矩阵 (nobody 403 / admin CRUD / kubelet
    node 写) → 非资源 URL allow → armed discovery
    (`rbac.authorization.k8s.io` + `serviceaccounts`) → TLS + kubectl
    kubeconfig 三态 (CA 信/无 CA 硬错/h2c 拒) → fail-closed。
  - golden `suites/auth.sh` **G107-G113**: SRV2 `--token-file` 三
    token CSV 的 wire 姿态 + G112 交叉断言主 server (未 arm) 不含
    RBAC/SA + G113 teardown 稳定性；默认 h2c 基线字节不动。
  - parity t78-auth (`server --token-file --tls-san`); 单测: rcgen
    生成/CSV 解析/authz 矩阵/kubeconfig 三态/server 优先级链/flannel
    kubeconfig 注入/kubelet 每 head 一枚 token stamp。

- **状态 / Status** — done (S50, 2026-11-21)
- **证据 / Evidence** — auth-e2e A1-A7 ✓; golden 75/75 (+G107-G113)
  ✓; parity 28/28 (t78-auth) ✓; workspace tests 1404/0 ✓;
  CHANGELOG S50。
- **卡点 / Blockers** — none
- **依赖 / Depends on** — T1.1
