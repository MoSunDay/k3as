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
  - Built on `kube-rs` core types; add our own schema registry for init-pro
    API groups (`init-pro.io/*`) and pass-through for native groups.
  - OpenAPI / swagger schema generation matching upstream so
    `kubectl explain` works.
  - etcd serialization: protobuf for native core types where upstream does
    (k8s `runtime.Serializer` parity), JSON otherwise.
  - `rest.Config` discovery compatible (kube-rs client resolves us).

- **验收手段 / Acceptance**
  - `kubectl get --raw` + `kube-rs` Api round-trip equality on a fixed
    object set (part of T0.6 golden).
  - `kubectl explain pods.spec` returns upstream-identical schema.

- **状态 / Status** — not-started
- **证据 / Evidence** — —
- **卡点 / Blockers** — Protobuf API fidelity is the hard part; decide
    whether v1 supports JSON-only and adds protobuf later.
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

- **状态 / Status** — not-started
- **证据 / Evidence** — —
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
