# e2e/ — manifest-driven end-to-end suites

## service-traffic e2e (`scripts/service-traffic-e2e.sh`, Sprint 18 / S6)

Boots a real single-node cluster (server + agent over the bundled
containerd), builds the offline pause + echo images (`scripts/build-*.sh`),
imports them through the staged `ctr`, then POSTs `e2e/manifests/*.json`
verbatim and asserts Service traffic end-to-end:

- **ST1** both echo replicas reach Running + Ready (phase + Ready condition).
- **ST2** the NodePort Service gets an auto-allocated `nodePort` in
  30000-32767 (S3; no explicit nodePort in the manifest).
- **ST3** Endpoints parity: subsets carry exactly the two podIPs (S1/S2)
  and port 8080 with name `web`.
- **ST4** 10 GETs through the nodePort all return 200 with `METHOD GET` +
  `PATH /probe`; at least 2 distinct `LOCAL` values (round-robin across
  replicas); a POST echoes `BODY sprint18`.
- **ST5** scaling the Deployment to zero converges the nodePort to 503
  (empty Endpoints -> router 503).
- **ST6** deleting the Service retires the listener (connection refused).

### Conventions
- **JSON-only manifests** (decision Q10): the wire format is JSON, so the
  manifests are canonical k8s JSON POSTed verbatim with curl — no YAML, no
  kubectl dependency.
- **`LOCAL`, not hostname, discriminates replicas**: the kubelet does not
  plumb `PodSandboxConfig.hostname` yet, so every pod inherits the node
  hostname; the echo image reports the accepted socket's local address
  (the podIP) as `LOCAL` — the unique per-replica discriminator.
- **Decision D**: NodePort-only service plane via the built-in Router
  (kube-proxy-equivalent listeners, one per allocated nodePort); there is
  no ClusterIP dataplane yet.

### SKIP conditions
The suite prints `SKIP service-traffic e2e (...)` and exits 0 when there is
no vendored containerd bundle (`vendor/bin`, or `INIT_PRO_VENDOR_BIN`) or
no `cc` for the offline image builders — same policy as golden G24/G25: a
missing vendor bundle is a build-configuration state, not a conformance
break (this is what CI hits with `INIT_PRO_VENDOR=0`).

### Run it
```sh
cargo build --workspace --locked   # prebuilt binary required
scripts/service-traffic-e2e.sh     # root; ~60s; loopback-only traffic
```

Complements — does not replace — `scripts/golden-conformance.sh` (T0.6
merge gate, G01-G25), which stays the immutable baseline.
