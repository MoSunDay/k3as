# kubelet (T4.2)

The kubelet equivalent: `crates/kubelet` (15th workspace crate),
dependency-light and pure-functional. It watches its assigned pods,
drives them to running over the T4.1 CRI seam, and reports pod + node
status back to the apiserver. Q26 is the CRI-client strategy, Q27 the
airgap workload image (`plans/init-pro/decisions.md`); as-built detail
in `plans/init-pro/plan/04-node.md` (T4.2). Scope A (pod lifecycle +
status) is done; probes/volumes/exec/pulls (Scope B/C) are not built.

## Shape (public API)

- `kubelet::spawn(cfg: KubeletConfig, cri: Arc<dyn CriBackend>,
  shutdown: infra::Shutdown) -> Vec<tokio::task::JoinHandle<()>>` —
  three long-running tasks; the caller owns the handles.
- `KubeletConfig::new(server_url, node_name, data_dir)`;
  `kubelet::default_node_name()`; sandbox image override via
  `INIT_PRO_SANDBOX_IMAGE`.
- Agent wiring (`crates/cli`): the kubelet spawns only when
  `--server` is `http://` — `https://` is rejected with a warning and
  the kubelet is skipped (the apiserver is HTTP-only, Q21).
  `--node-name` flag added. Drain order: kubelet → runtime
  (containerd) → apiserver → controllers → scheduler.

## The three tasks

1. **watch** — own minimal HTTP/1.1 client + chunked framing
   (`http.rs`, `framing.rs`, `watch.rs`): LIST then watch
   `/api/v1/pods`, maintaining a desired map filtered by
   `spec.nodeName == node_name`.
2. **sync** — level-driven: diff desired vs a CRI snapshot →
   sandbox/container create/start/stop/remove; then a status pass
   PUTting `pods/status` only when semantically changed.
3. **node** — registers the Node object and heartbeats the
   `kube-node-lease` Lease.

## Status model

- `phase` Running/Pending; conditions `PodScheduled` + `Ready`;
  `containerStatuses` with `containerID` `cri://<id>` and
  `restartCount` = crictl attempt number.
- Backed by a new apiserver subresource `PUT pods/status`
  (`crates/apiserver/src/pod_status.rs`): read-first, merges ONLY
  `.status`, CAS on the entry's `mod_revision`.
- Sprint 18 (S1): podIP/podIPs + hostIP=127.0.0.1 emission — every
  SANDBOX_READY sandbox is enriched with `crictl inspectp -o json`
  (`runtime::cri_json::PodSandboxInspect` + `cri.rs`
  `inspect_pod_sandbox`), and `status.rs` surfaces
  `status.network.ip`; podIP is part of the semantic-change key so a
  late-arriving CNI address re-triggers the PUT. The Node object
  advertises an InternalIP address (`objects.rs`).

## CRI seam + airgap image (Q26/Q27)

- CRI via the `crates/runtime` `CriCtl` backend (vendored-crictl
  subprocess, Q26 route B) behind the `CriBackend` trait.
- Workload image (registries blocked in this env): static pause
  (`gcc -static -Os`) + hand-assembled OCI layout via
  `scripts/build-pause-image.sh` → `init-pro.local/pause:0.1`,
  imported through the staged `ctr` — no registry access.
- CNI: containerd CRI requires an eth0 IP on sandboxes, so the
  runtime conflist is bridge + host-local 10.42.0.0/24 (not
  loopback).

## End-to-end (the G25 flow)

```sh
cargo build --workspace --locked
scripts/build-pause-image.sh /tmp/pause.tar init-pro.local/pause:0.1
./target/debug/init-pro server --data-dir /tmp/srv --bind-address 127.0.0.1 --https-listen-port 6443 &
INIT_PRO_SANDBOX_IMAGE=init-pro.local/pause:0.1 ./target/debug/init-pro agent \
    --data-dir /tmp/ag --server http://127.0.0.1:6443 --node-name node1 &
INIT_PRO_DATA_DIR=/tmp/ag ./target/debug/init-pro ctr -n k8s.io images import /tmp/pause.tar
./target/debug/init-pro kubectl apply -f deployment.yaml   # -> Running+Ready
```

## Known limits (Scope B/C, not built)

Probes, volumes, exec/logs/attach, real image pulls, node
capacity/status richness.

## Proof

- 56 tests in `crates/kubelet/` (incl. a fake-CRI end-to-end over a
  real HTTP fake apiserver); 674 workspace green (Sprint 18: +3
  status tests for podIP/podIPs/hostIP).
- Golden G25 — 3 assertions: pod Running+Ready on the agent node;
  killed container restarted with a new id; Deployment delete → zero
  sandboxes. Gated on the vendor bundle + `cc`, SKIPs otherwise.
