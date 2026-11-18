#!/usr/bin/env bash
# T4.13 (Q53/Q54): shared helpers for scripts/builtin-image-e2e.sh.
# SOURCED (never executed) by the main script, which must first set:
#   BIN       the init-pro multicall binary
#   BUSYBOX   a STATIC busybox
#   DD        scratch dir (removed on exit)
#   KUBECTL   kubectl symlink alias (Q1 multicall)
# and after boot_cluster: BASE, REG, NODE, SRV_LOG, AGENT_LOG,
# API_PORT, REG_PORT, SRV_PID, AGENT_PID.

# --- assertions ------------------------------------------------------------
bimg_fail() { echo "FAIL $*" >&2; exit 1; }
bimg_ok() { echo "OK   $*"; }

# --- ports -----------------------------------------------------------------
pick_port() {
  python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()'
}

# --- cluster lifecycle -----------------------------------------------------
bimg_init_traps() { # install the EXIT/HUP/INT/TERM teardown (uses DD, PIDs)
  bimg_cleanup() {
    [[ -n "${AGENT_PID:-}" ]] && kill "$AGENT_PID" 2>/dev/null || true
    [[ -n "${SRV_PID:-}" ]] && kill "$SRV_PID" 2>/dev/null || true
    [[ -n "${AGENT_PID:-}" ]] && wait "$AGENT_PID" 2>/dev/null || true
    [[ -n "${SRV_PID:-}" ]] && wait "$SRV_PID" 2>/dev/null || true
    rm -rf "${DD:-}" 2>/dev/null || true
  }
  trap bimg_cleanup EXIT
  trap 'exit 129' HUP
  trap 'exit 130' INT
  trap 'exit 143' TERM
}

boot_cluster() { # server (embedded registry) + builtin agent; sets SRV_PID/AGENT_PID
  "$BIN" server --data-dir "$DD/srv" --bind-address 127.0.0.1 \
    --https-listen-port "$API_PORT" \
    --registry-addr "127.0.0.1:${REG_PORT}" >"$SRV_LOG" 2>&1 &
  SRV_PID=$!
  for _ in $(seq 1 100); do
    grep -q "discovery listening" "$SRV_LOG" && break; sleep 0.1
  done
  grep -q "discovery listening" "$SRV_LOG" \
    || bimg_fail "server never reported listening ($(tail -5 "$SRV_LOG" | tr '\n' ' '))"
  for _ in $(seq 1 100); do
    [[ "$(curl -s -o /dev/null -w '%{http_code}' "$REG/v2/" 2>/dev/null)" == "200" ]] && break; sleep 0.1
  done
  [[ "$(curl -s -o /dev/null -w '%{http_code}' "$REG/v2/")" == "200" ]] \
    || bimg_fail "embedded registry never answered at $REG"
  "$BIN" agent --data-dir "$DD/agent" --token dev --server "$BASE" \
    --node-name "$NODE" --container-engine builtin \
    --registry-addr "127.0.0.1:${REG_PORT}" >"$AGENT_LOG" 2>&1 &
  AGENT_PID=$!
  for _ in $(seq 1 300); do
    grep -q "kubelet started (T4.12 built-in engine)" "$AGENT_LOG" && break; sleep 0.1
  done
  grep -q "kubelet started (T4.12 built-in engine)" "$AGENT_LOG" \
    || bimg_fail "builtin agent never came up ($(tail -5 "$AGENT_LOG" | tr '\n' ' '))"
}

# --- image fabrication + push ----------------------------------------------
# build_image SPEC_FILE: fabricate a docker-v2 image from a JSON spec:
#   {"out": "<dir>", "layers": [[["path","file","content"|"@busybox","0644"],
#                                ["path","symlink","target"]], ...],
#    "config": {"Env": [...], "Entrypoint": [...], "Cmd": [...],
#               "WorkingDir": "...", "User": "..."}}
# Writes layerN.tar.gz + config.json + manifest.json under `out` and
# prints "<cfg-d> <cfg-s> <l1-d> <l1-s> [<l2-d> <l2-s> ...]".
build_image() {
  python3 - "$1" "$BUSYBOX" <<'PY'
import gzip, hashlib, io, json, os, sys, tarfile

spec_path, busybox = sys.argv[1], sys.argv[2]
spec = json.load(open(spec_path))
out = spec["out"]
with open(busybox, "rb") as f:
    bb = f.read()

def sha256_bytes(b):
    return "sha256:" + hashlib.sha256(b).hexdigest()

layers, diff_ids = [], []
for i, entries in enumerate(spec["layers"]):
    raw = io.BytesIO()
    with tarfile.open(fileobj=raw, mode="w") as tf:
        for entry in entries:
            path, kind, data = entry[0], entry[1], entry[2]
            mode = int(entry[3], 8) if len(entry) > 3 else 0o644
            ti = tarfile.TarInfo(path)
            if kind == "symlink":
                ti.type = tarfile.SYMTYPE
                ti.linkname = data
                ti.mode = 0o777
            else:
                if data == "@busybox":
                    data = bb
                elif isinstance(data, str):
                    data = data.encode()
                ti.size = len(data)
                ti.mode = mode
            tf.addfile(ti, io.BytesIO(data) if kind == "file" else None)
    tar_raw = raw.getvalue()
    blob = gzip.compress(tar_raw, mtime=0)
    with open(os.path.join(out, "layer%d.tar.gz" % i), "wb") as f:
        f.write(blob)
    layers.append((sha256_bytes(blob), len(blob)))
    diff_ids.append(sha256_bytes(tar_raw))

cfg = {"architecture": "amd64", "os": "linux", "config": spec.get("config", {}),
       "rootfs": {"type": "layers", "diff_ids": diff_ids}, "history": []}
cfg_b = json.dumps(cfg, separators=(",", ":")).encode()
with open(os.path.join(out, "config.json"), "wb") as f:
    f.write(cfg_b)

manifest = {
    "schemaVersion": 2,
    "mediaType": "application/vnd.docker.distribution.manifest.v2+json",
    "config": {"mediaType": "application/vnd.docker.container.image.v1+json",
               "size": len(cfg_b), "digest": sha256_bytes(cfg_b)},
    "layers": [{"mediaType": "application/vnd.docker.image.rootfs.diff.tar.gzip",
                "size": size, "digest": digest} for digest, size in layers],
}
with open(os.path.join(out, "manifest.json"), "w") as f:
    json.dump(manifest, f, separators=(",", ":"))
print(sha256_bytes(cfg_b), len(cfg_b),
      " ".join("%s %d" % (d, s) for d, s in layers))
PY
}

push_blob() { # REPO FILE DIGEST -> echoes the POST status (201 = ok)
  curl -s -o /dev/null -w '%{http_code}' -X POST \
    -H 'Content-Type: application/octet-stream' \
    --data-binary "@$2" "$REG/v2/$1/blobs/uploads/?digest=$3"
}

put_manifest() { # REPO MANIFEST_FILE TAG -> echoes the PUT status
  curl -s -o "$DD/manifest-put.out" -w '%{http_code}' -X PUT \
    -H 'Content-Type: application/vnd.docker.distribution.manifest.v2+json' \
    --data-binary "@$2" "$REG/v2/$1/manifests/$3"
}

# push_image OUT_DIR REPO TAG SPEC_FILE: build + push config & layers +
# manifest; asserts every hop (fails the script on a non-201). Digests
# contain ':' so the blob pairs use '|' as their separator. Records the
# pushed descriptors in globals for the white-box cases: BIMG_CONFIG_DIGEST
# and the array BIMG_LAYER_DIGESTS (layer order).
push_image() {
  local out="$1" repo="$2" tag="$3" spec="$4" code digest file i
  mkdir -p "$out"
  read -r -a words <<<"$(build_image "$spec")"
  [[ ${#words[@]} -ge 4 ]] || bimg_fail "image builder produced no digests"
  local blobs=("${words[0]}|$out/config.json")
  for ((i = 0; 2 + 2 * i + 1 < ${#words[@]}; i++)); do
    blobs+=("${words[2 + 2 * i]}|$out/layer$i.tar.gz")
  done
  BIMG_CONFIG_DIGEST="${words[0]}"
  BIMG_LAYER_DIGESTS=()
  for ((i = 0; 2 + 2 * i + 1 < ${#words[@]}; i++)); do
    BIMG_LAYER_DIGESTS+=("${words[2 + 2 * i]}")
  done
  for f_d in "${blobs[@]}"; do
    digest="${f_d%%|*}"; file="${f_d#*|}"
    code="$(push_blob "$repo" "$file" "$digest")"
    [[ "$code" == "201" ]] || bimg_fail "blob POST $file ($digest) -> $code (expected 201)"
  done
  code="$(put_manifest "$repo" "$out/manifest.json" "$tag")"
  [[ "$code" == "201" ]] \
    || bimg_fail "manifest PUT $repo:$tag -> $code (expected 201): $(head -c 200 "$DD/manifest-put.out")"
}

delete_registry_blob() { # REPO DIGEST -> status (202 = accepted, T6.3 CAS)
  curl -s -o /dev/null -w '%{http_code}' -X DELETE "$REG/v2/$1/blobs/$2"
}

# --- pod helpers -----------------------------------------------------------
post_pod() { # NAME IMAGE ARGS_JSON -> echoes the POST status (command pod)
  curl -s -o /dev/null -w '%{http_code}' -H 'Content-Type: application/json' \
    --data-binary @- "$BASE/api/v1/namespaces/default/pods" <<PODJSON
{"apiVersion":"v1","kind":"Pod","metadata":{"name":"$1","namespace":"default"},
 "spec":{"containers":[{"name":"c","image":"$2","command":["/bin/busybox"],"args":$3}]}}
PODJSON
}

post_pod_json() { # NAME POD_JSON -> echoes the POST status (full body)
  curl -s -o /dev/null -w '%{http_code}' -H 'Content-Type: application/json' \
    --data-binary "$2" "$BASE/api/v1/namespaces/default/pods"
}

pod_phase() { # NAME -> current phase ("" on error)
  curl -s "$BASE/api/v1/namespaces/default/pods/$1" | python3 -c '
import json,sys
try: print(json.load(sys.stdin)["status"].get("phase",""))
except Exception: print("")
' 2>/dev/null || true
}

wait_phase() { # NAME WANT TENTHS -> final phase (0.5s per tenth)
  local phase=""
  for _ in $(seq 1 "$3"); do
    phase="$(pod_phase "$1")"
    [[ "$phase" == "$2" ]] && break
    sleep 0.5
  done
  echo "$phase"
}

wait_logs() { # NAME PATTERN TENTHS -> logs once the pattern shows up
  local logs=""
  for _ in $(seq 1 "$3"); do
    logs="$("$KUBECTL" logs "$1" --server "$BASE" 2>/dev/null || true)"
    grep -q "$2" <<<"$logs" && break
    sleep 0.5
  done
  echo "$logs"
}

# --- generic REST + service helpers (M5) ------------------------------------
api_post_json() { # PATH BODY -> POST status
  curl -s -o /dev/null -w '%{http_code}' -H 'Content-Type: application/json' \
    --data-binary "$2" "$BASE/$1"
}

api_delete() { # PATH -> DELETE status
  curl -s -o /dev/null -w '%{http_code}' -X DELETE "$BASE/$1"
}

delete_pod() { # NAME -> DELETE + bounded wait until gone (port frees)
  local code i
  code="$(api_delete "api/v1/namespaces/default/pods/$1")"
  [[ "$code" == "200" || "$code" == "404" ]] || bimg_fail "pod DELETE $1 -> $code"
  for i in $(seq 1 60); do
    [[ "$(pod_phase "$1")" == "" ]] && break
    sleep 0.5
  done
}

wait_port_free() { # PORT -> fail if still answering after the bounded wait
  local i
  for i in $(seq 1 60); do
    curl -fsS -m 1 "http://127.0.0.1:$1/" >/dev/null 2>&1 || return 0
    sleep 0.5
  done
  bimg_fail "port $1 still open after cleanup"
}

svc_node_port() { # NAME PORTNAME -> allocated nodePort ("" when none)
  curl -s "$BASE/api/v1/namespaces/default/services/$1" | python3 -c '
import json,sys
try: svc=json.load(sys.stdin)
except Exception: sys.exit(0)
for p in svc.get("spec",{}).get("ports",[]):
    if p.get("name")=="'"$2"'" and p.get("nodePort") is not None: print(p["nodePort"]); break' 2>/dev/null || true
}

# --- agent restart (M5: CAS persistence + startup-GC triggers) --------------
restart_agent() { # EXTRA_ARGS... : stop the agent, relaunch on the SAME
  # data dir with the base flags + extras; waits for the kubelet marker in
  # a FRESH log so stale greps cannot lie. Updates AGENT_PID/AGENT_LOG.
  local i
  if [[ -n "${AGENT_PID:-}" ]]; then
    kill -TERM "$AGENT_PID" 2>/dev/null || true
    for i in $(seq 1 40); do kill -0 "$AGENT_PID" 2>/dev/null || break; sleep 0.25; done
    kill -KILL "$AGENT_PID" 2>/dev/null || true
    wait "$AGENT_PID" 2>/dev/null || true
  fi
  AGENT_RESTARTS=$(( ${AGENT_RESTARTS:-0} + 1 ))
  AGENT_LOG="$DD/agent${AGENT_RESTARTS}.log"
  "$BIN" agent --data-dir "$DD/agent" --token dev --server "$BASE" \
    --node-name "$NODE" --container-engine builtin \
    --registry-addr "127.0.0.1:${REG_PORT}" "$@" >"$AGENT_LOG" 2>&1 &
  AGENT_PID=$!
  for i in $(seq 1 300); do
    grep -q "kubelet started (T4.12 built-in engine)" "$AGENT_LOG" && break; sleep 0.1
  done
  grep -q "kubelet started (T4.12 built-in engine)" "$AGENT_LOG" \
    || bimg_fail "agent restart never came up ($(tail -5 "$AGENT_LOG" | tr '\n' ' '))"
}
