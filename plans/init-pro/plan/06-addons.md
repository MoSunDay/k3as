# Layer 6 — Addons / Manifests

Mirrors `index.md` TODO IDs **T6.1–T6.2**. Q2-compatible: addons ship as
standard manifests/CRDs that `kubectl`/`helm` see natively. Parity with
k3s `pkg/deploy/` auto-deploy machinery.

---

## T6.1 — manifest 自动部署 (HelmChart auto-deploy)

- **目标 / Goal**
  Auto-deploy manifests dropped into the data dir + a `HelmChart` CRD that
  init-pro reconciles (k3s `pkg/deploy/controller.go` parity).

- **核心实现 / Core implementation**
  - Controller watches `<data-dir>/server/manifests/*.yaml` and applies on
    change (k3s deploy/ pkg model).
  - `HelmChart` CRD (k3s `helm.cattle.io/v1`); bundled `helm` (multicall
    alias) renders charts server-side.
  - `--disable` gating (T0.4) for the standard chart set.

- **验收手段 / Acceptance**
  - Golden: drop a manifest → it appears via `kubectl get`; create a
    `HelmChart` → its resources render.

- **状态 / Status** — not-started
- **证据 / Evidence** — —
- **卡点 / Blockers** — none
- **依赖 / Depends on** — T1.2, T3.1

---

## T6.2 — 标准 addon (CoreDNS/local-storage/metrics-server)

- **目标 / Goal**
  The k3s default addon stack, toggle-able via `--disable` (T0.4):
  `coredns`, `local-storage`, `metrics-server` (and `servicelb`/`traefik`
  superseded by Layers 5/4.3).

- **核心实现 / Core implementation**
  - Bundled manifests pinned to upstream versions.
  - CoreDNS: standard Deployment + ConfigMap.
  - local-storage: a `StorageClass` + provisioner for node-local dirs.
  - metrics-server: upstream manifest; resource policy to run on control
    plane nodes.

- **验收手段 / Acceptance**
  - Golden: with defaults, DNS resolves, a PVC binds on `local-storage`,
    `kubectl top nodes` returns metrics; each `--disable` removes its piece.

- **状态 / Status** — not-started
- **证据 / Evidence** — —
- **卡点 / Blockers** — none
- **依赖 / Depends on** — T6.1
