# Runbook: migrate node04 llama.cpp serving into k3as (T7.6, Q62)

Migrates an operator-run `llama-server` systemd unit on `node04` to an
ordinary `apps/v1` Deployment (Recreate) + NodePort Service using the
T4.15 host-direct profile (Q60) and T7.2 whole-GPU requests (Q61).
No Agent, no CRD, no model-aware routing; the generic NodePort data
plane serves the OpenAI-compatible API.

Scope guards:
- node04 is a **tenant-dedicated** node; Q60/Q61 are performance
  profiles, not containment. Never share the node across tenants.
- INF6 evidence rules: assets are preinstalled and pinned — this
  runbook never downloads drivers, llama.cpp, or GGUF.
- Wire is JSON-only (Q10); this build's `kubectl apply` covers
  argoproj.io/init-pro.io only, so Deployment/Service lifecycle goes
  through `curl` against the apiserver.

Prereq docs: `inventory-template.md` (phase 0), parameterized
manifests in `manifests/`.

## Phase 0 — inventory & preflight

1. Copy `inventory-template.md` to `inventory-node04.md`; fill every
   row, attach `*evidence` outputs. STOP if any row is unknown.
2. Confirm the agent on node04 runs the builtin engine with host
   rootfs and GPU inventory (`--container-engine builtin
   --builtin-rootfs host --gpu-inventory auto`), and the node
   publishes `@GPU_RESOURCE@` capacity/allocatable:
   `curl -s "$BASE/api/v1/nodes/$NODE_NAME"`.
3. Operator host tooling: `python3 -c 'import yaml'`, `curl`,
   `sha256sum`; ssh access to the node for `systemctl`.
4. Create the evidence dir and run every later phase from the ops
   root so relative manifest paths resolve:
   `mkdir -p evidence` (all commands below assume cwd
   `plans/init-pro/ops/`); record every verification as
   `<cmd> 2>&1 | tee evidence/<step>.log`.

## Phase 1 — prestage pinned assets (node04)

    sha256sum "$LLAMA_SERVER_BIN" | tee llama-bin.sha256
    sha256sum "$GGUF_HOST_DIR/$MODEL_ALIAS.gguf" | tee gguf.sha256
    chmod 0444 "$GGUF_HOST_DIR/$MODEL_ALIAS.gguf"   # immutable (Q62)
    stat -c '%a' "$GGUF_HOST_DIR/$MODEL_ALIAS.gguf"  # expect 444

Read-only exposure is by the `init-pro.io/host-mounts: /models=…:ro`
annotation — no runtime model mutation is possible through the pod.

## Phase 2 — apply at replicas 0 (idle control plane)

Apply the Service FIRST so the empty-Endpoints state is observed
deterministically (INF1/IF2 shape), then the Deployment pinned at
`replicas: 0` — nothing is scheduled, the GPU is still owned by the
old unit.

    render() { sed -e "s|@NODE_NAME@|$NODE_NAME|g" \
        -e "s|@LLAMA_SERVER_BIN@|$LLAMA_SERVER_BIN|g" \
        -e "s|@GGUF_HOST_DIR@|$GGUF_HOST_DIR|g" \
        -e "s|@MODEL_ALIAS@|$MODEL_ALIAS|g" -e "s|@PORT@|$PORT|g" \
        -e "s|@GPU_RESOURCE@|$GPU_RESOURCE|g" -e "s|@REPLICAS@|0|g" \
        "$@"; }
    render manifests/llama-service.yaml | sed -e "s|@NODE_PORT@|$NODE_PORT|g" \
      | python3 -c 'import json,sys,yaml;print(json.dumps(yaml.safe_load(sys.stdin)))' \
      | curl -s -w '%{http_code}\n' -X POST -H 'Content-Type: application/json' \
          --data-binary @- "$BASE/api/v1/namespaces/$NS/services"
    render manifests/llama-deployment.yaml \
      | python3 -c 'import json,sys,yaml;print(json.dumps(yaml.safe_load(sys.stdin)))' \
      | curl -s -w '%{http_code}\n' -X POST -H 'Content-Type: application/json' \
          --data-binary @- "$BASE/apis/apps/v1/namespaces/$NS/deployments"

Both must return `201`. Verify idle:

    curl -s "$BASE/api/v1/namespaces/$NS/endpoints/llama"   # addresses: []
    curl -s "http://127.0.0.1:$NODE_PORT/health"            # 503 (no upstream peers)

## Phase 3 — GPU hand-over from the old unit (on node04)

    sudo systemctl stop "$OLD_UNIT"
    sudo systemctl disable "$OLD_UNIT"
    pgrep -af llama-server || true        # must list nothing
    ss -ltnp | grep ":$PORT " || true     # must list nothing

Keep the unit file + old asset for rollback (record in inventory).

## Phase 4 — scale to 1 (INF4 hold applies to later updates)

    curl -s -X PATCH -H 'Content-Type: application/merge-patch+json' \
      -d '{"spec":{"replicas":1}}' \
      "$BASE/apis/apps/v1/namespaces/$NS/deployments/llama"

The pod must land on `$NODE_NAME` (nodeSelector) as a host-direct pod
and allocate 1 whole GPU from the node ledger.

INF4 semantics for every LATER spec change (new GGUF, new pinned
binary, port change): `strategy: Recreate` scales the old RS to zero
and holds the NEW ReplicaSet at zero until BOTH (a) the old pod object
is gone AND (b) the hosting node acknowledged runtime teardown +
ledger release for the old podUID. Status while held:
`Progressing=False, reason=OldRuntimeTeardownPending` (message names
`pod-terminating` / `release-unacked` / `ledger-referenced`). A
stale release ack never unblocks (timestamp must be newer than the
hold's `init-pro.io/teardown-since`). Do not hand-edit
`teardown-since`/`teardown-pending` annotations; fix the node side.

前置条件（S47 评审 F3）：控制面与 node04 的 NTP 偏差必须 ≤ 60s（确认
margin = max(2×10s 心跳默认, 60s)）；超差时 teardown-hold 确认被延迟，
`OldRuntimeTeardownPending` 消息携带 ack-at / teardown-since / margin
供现场比对两侧时钟。

## Phase 5 — verification checklist (record each as evidence)

| # | Check (INF gate) | Command / expectation |
|---|---|---|
| 1 | Pod Running+Ready (INF1) | `GET /api/v1/namespaces/$NS/pods?labelSelector=app%3Dllama` → phase Running, Ready condition True |
| 2 | Endpoints populated (INF1) | `GET .../endpoints/llama` → 1 address on port `$PORT` |
| 3 | Health via NodePort (INF2) | `curl http://127.0.0.1:$NODE_PORT/health` → `{"status":"ok"}`-shaped 200 |
| 4 | Models (INF2) | `curl .../v1/models` lists `$MODEL_ALIAS` |
| 5 | Chat non-stream (INF2) | `POST .../v1/chat/completions` `{"model":"$MODEL_ALIAS","messages":[…]}` → JSON `choices[0].message.content` |
| 6 | Chat stream (INF3) | same + `"stream":true` → `data:` SSE deltas in order, final event `data: [DONE]` and nothing after |
| 7 | GPU env injected (Q61) | pod log contains `CUDA_VISIBLE_DEVICES` (NVIDIA) or `HIP_VISIBLE_DEVICES` (AMD) with the assigned device |
| 8 | Ledger allocation | node status `status.gpuLedger.$GPU_RESOURCE.allocations` references the pod's UID |
| 9 | No model-aware routing | `GET /apis/networking.k8s.io/v1/.../ingresses` and `.../trafficsplits` lists are empty |
| 10 | No image pull | pod log has no pull attempt for `init-pro.local/llama:$MODEL_ALIAS` |

Pod log: `GET /api/v1/namespaces/$NS/pods/<pod>/log?container=server`
(fetch the FULL log; readiness probes add one line/s).

## Phase 6 — observation

Soak ≥ 30 min: repeat checks 3-6 periodically, watch
`GET .../deployments/llama` stays available, and the pod log stays
free of restart/error lines. Only then repoint downstream consumers
from the old unit's address to `http://<node>:$NODE_PORT`.

## Rollback (return to the systemd unit)

1. Scale down first — the GPU must be released by the runtime, not by
   killing the unit's competition:

       curl -s -X PATCH -H 'Content-Type: application/merge-patch+json' \
         -d '{"spec":{"replicas":0}}' \
         "$BASE/apis/apps/v1/namespaces/$NS/deployments/llama"

2. Wait for BOTH release signals before touching the old unit (the
   same ack INF4 waits on):

       while :; do
         OUT=$(python3 -c '
       import json, sys
       node, eps = json.loads(sys.argv[1]), json.loads(sys.argv[2])
       allocs = node["status"].get("gpuLedger", {}).get(sys.argv[3], {}).get("allocations", {})
       addrs = sum(len(s.get("addresses") or []) for s in eps.get("subsets", []))
       print("released" if not allocs and addrs == 0 else
             "waiting: allocations=%s endpoints=%s" % (list(allocs), addrs))
       ' "$(curl -s "$BASE/api/v1/nodes/$NODE_NAME")" \
         "$(curl -s "$BASE/api/v1/namespaces/$NS/endpoints/llama")" \
         "$GPU_RESOURCE")
         echo "$OUT"; [ "$OUT" = released ] && break; sleep 2
       done

   Also confirm the node's `status.released[]` carries the podUID with
   a fresh `at` timestamp. If allocations never drain, the node agent
   is stuck — investigate the agent log; do NOT start the old unit
   while an allocation exists (whole-GPU is logical exclusivity, Q61).
3. Restart the old service and verify from a consumer:

       sudo systemctl enable --now "$OLD_UNIT"
       curl "http://127.0.0.1:$OLD_PORT/health"

4. Leave the k3as objects at replicas 0 (or DELETE both) and record
   the rollback evidence.

## Gotchas that bite (all observed live)

- GPU request/limit quantities MUST be quoted strings (`"1"`);
  numeric JSON rows are silently dropped and the pod runs WITHOUT a
  GPU allocation.
- `nsrun` exec does no PATH resolution — `command`/`args` use
  absolute paths only.
- Host-direct pods share the host network namespace (Q52): `$PORT`
  must be free ON THE NODE and `targetPort` must equal it exactly.
- Readiness `httpGet` probes log one line per second per probe.
- This build's `kubectl apply` does not accept `apps/v1`; use curl.
