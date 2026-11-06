# kubelet (T4.2)

The kubelet equivalent: `crates/kubelet` (joined as the 15th workspace
crate; the workspace now has 16 members),
dependency-light and pure-functional. It watches its assigned pods,
drives them to running over the T4.1 CRI seam, and reports pod + node
status back to the apiserver. Q26 is the CRI-client strategy, Q27 the
airgap workload image (`plans/init-pro/decisions.md`); as-built detail
in `plans/init-pro/plan/04-node.md` (T4.2). Scope A+B+C all done —
T4.2 closed at Sprint 24 (Q31): Scope B (Sprint 20) HTTP probe engine,
emptyDir/configMap/secret volumes, imagePullPolicy, hostname (golden
G26); Scope C (Sprint 24) `kubectl logs`/`exec` end-to-end over the
apiserver relay -> kubelet listener -> crictl (golden G30), with
`scripts/local-up.sh` promoted to the official single-node UX. See
`agents.md` and root `CHANGELOG.md` (Sprint 24).

## Shape (public API)

- `kubelet::spawn(cfg: KubeletConfig, cri: Arc<dyn CriBackend>,
  shutdown: infra::Shutdown) -> Vec<tokio::task::JoinHandle<()>>` —
  five long-running loops (watch, sync, node/lease, local log/exec
  listener, and since T4.6/Q39 the copytruncate log-rotation sweep);
  the caller owns the handles.
- `KubeletConfig::new(server_url, node_name, data_dir)`;
  `kubelet::default_node_name()`; sandbox image override via
  `INIT_PRO_SANDBOX_IMAGE`.
- Agent wiring (`crates/cli`): the kubelet spawns only when
  `--server` is `http://` — `https://` is rejected with a warning and
  the kubelet is skipped (the apiserver is HTTP-only, Q21).
  `--node-name` flag added. Drain order: kubelet → runtime
  (containerd) → apiserver → controllers → scheduler.

## The four loops

1. **watch** — own minimal HTTP/1.1 client + chunked framing
   (`http.rs`, `framing.rs`, `watch.rs`): LIST then watch
   `/api/v1/pods`, maintaining a desired map filtered by
   `spec.nodeName == node_name`.
2. **sync** — level-driven: diff desired vs a CRI snapshot →
   sandbox/container create/start/stop/remove; then a status pass
   PUTting `pods/status` only when semantically changed.
3. **node** — registers the Node object and heartbeats the
   `kube-node-lease` Lease.
4. **local logs+exec listener** — since T4.2 Scope C (Q31): a plain-HTTP
   server on `<node-ip>:<kubelet-port>` (`server.rs`) serving
   `/pods/{pod_uid}/containers/{container}/log|exec`; the apiserver
   relay dials it instead of shelling crictl locally.
5. **log rotation sweep** — since T4.6 (Q39): logs live in date
   directories `<data_dir>/pods/<pod-uid>/<YYYY-MM-DD>/<container>.
   <attempt>.log` (date pinned at plan time); a 60s sweep copytruncates
   any live file >= 100 MiB (5 generations retained, `log_rotate.rs`),
   never touching the `volumes/` sibling tree.

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
- CNI: containerd CRI requires an eth0 IP on sandboxes. Since T4.3
  Scope A (Q34) the staged conflist is `10-flannel.conflist`
  (runtime/stage.rs) — the flannel meta-plugin delegating to
  bridge/host-local — so each node hands out IPs from its own
  `spec.podCIDR` /24, allocated by the node_cidr controller from
  `--cluster-cidr` (default 10.42.0.0/16) instead of one fixed subnet.

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

## Known limits

Real image pulls, node capacity/status richness; no `attach` or
port-forward — the relay is logs/exec only (documented Q31 deviation).

## Proof

- ~139 tests in `crates/kubelet/` (91 in `src/` + 48 in `tests/`, incl.
  a fake-CRI end-to-end over a real HTTP fake apiserver); workspace
  tally now ~937 (the Sprint 18 bump was +3 status tests for
  podIP/podIPs/hostIP).
- Golden G25 — 3 assertions: pod Running+Ready on the agent node;
  killed container restarted with a new id; Deployment delete → zero
  sandboxes. Gated on the vendor bundle + `cc`, SKIPs otherwise.
