# node04 llama migration — phase-0 inventory template (T7.6, Q62)

Copy to `inventory-node04.md`, fill EVERY field, and attach recorded
evidence (command + output) for the `*evidence` rows. A migration never
starts with an unfilled or stale row. No field may contain a secret,
token, or credential.

## Cluster / access

| Field | Example | Value |
|---|---|---|
| `BASE` (apiserver http base) | `http://127.0.0.1:6443` | |
| `NAMESPACE` | `default` | |
| `NODE_NAME` (target GPU node) | `node04` | |
| node is tenant-dedicated (Q60/Q61) | yes/no | |
| agent runs builtin engine, host rootfs | `--container-engine builtin --builtin-rootfs host` | |
| agent GPU inventory flag | `--gpu-inventory auto` (real) or `fake:...` | |
| node publishes `@GPU_RESOURCE@` capacity *evidence* | `curl $BASE/api/v1/nodes/$NODE_NAME` | |

## Workload parameters (manifest placeholders)

| Placeholder | Example | Value |
|---|---|---|
| `@GPU_RESOURCE@` | `nvidia.com/gpu` or `amd.com/gpu` | |
| `@MODEL_ALIAS@` (also the GGUF basename) | `qwen3-8b` | |
| `@PORT@` container listen port (Q52 host-netns: must be free ON THE NODE) | `18434` | |
| `@NODE_PORT@` static NodePort 30000-32767 | `30443` | |
| `@REPLICAS@` (start `0`; runbook scales) | `0` | |

## Pinned assets (INF6: no download at migration time)

| Field | Example | Value |
|---|---|---|
| `@LLAMA_SERVER_BIN@` absolute path | `/opt/llama/llama-server` | |
| llama-server pinned version | `b<major>-<hash>` | |
| llama-server sha256 *evidence* | `sha256sum` output | |
| `@GGUF_HOST_DIR@` host dir holding the model | `/srv/models` | |
| GGUF file | `/srv/models/@MODEL_ALIAS@.gguf` | |
| GGUF sha256 + mode 0444 (immutable) *evidence* | `sha256sum` + `stat -c %a` | |

## Old service being replaced

| Field | Example | Value |
|---|---|---|
| systemd unit | `llama-server.service` | |
| old listen addr:port | `0.0.0.0:8080` | |
| old clients / downstream consumers | list | |
| rollback image/asset retained | yes/no | |

## Tooling prerequisites on the operator host

- [ ] `python3` with `yaml` (PyYAML) — the runbook renders YAML→JSON (Q10)
- [ ] `curl`, `sha256sum`, `systemctl` (on the node, via ssh)
- [ ] evidence directory created and writable (e.g. `evidence/`)
