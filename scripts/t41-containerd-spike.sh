#!/usr/bin/env bash
# T4.1 timeboxed risk spike (Q24): vendor -> stage -> supervise -> CRI.
#
# Proves the longest unstarted chain to M3 with the already-acquired pinned
# artifacts (containerd 1.7.20, runc 1.1.13, cni-plugins 1.5.1):
#   1. stage the runtime binaries k3s-style into <dd>/agent/containerd/
#   2. write a k3s-style minimal config.toml
#   3. boot containerd THROUGH THE MULTICALL SEAM (`init-pro containerd ...`
#      execs the staged binary, Q24) and supervise it
#   4. verify: daemon answers (`ctr version` via the same seam), the CRI
#      plugin reports ready (`ctr plugins ls`), a real runc container runs
#      (image pull is best-effort: registries may be unreachable).
#
# This is a spike, not production wiring: T4.1 stays in-progress; findings
# are recorded in plans/init-pro/decisions.md (Q24).
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
BIN="${INIT_PRO_BIN:-$TARGET_DIR/debug/init-pro}"
VENDOR="$ROOT/vendor/bin"

[[ -x "$BIN" ]] || { echo "error: $BIN not found. Run 'cargo build'." >&2; exit 1; }
for f in containerd ctr runc; do
  [[ -x "$VENDOR/$f" ]] || { echo "error: $VENDOR/$f missing. Run 'INIT_PRO_VENDOR=1 cargo build -p init-pro'." >&2; exit 1; }
done

DD="${T41_DATA_DIR:-$(mktemp -d)}"
RUNTIME_DIR="$DD/agent/containerd"
ETC_DIR="$DD/agent/etc/containerd"
RUN_DIR="$DD/run/containerd"
SOCK="$RUN_DIR/containerd.sock"

pass=0; fail=0
ok()  { echo "OK   $1"; pass=$((pass+1)); }
bad() { echo "FAIL $1"; fail=$((fail+1)); }

cleanup() {
  [[ -n "${SRV:-}" ]] && { kill "$SRV" 2>/dev/null || true; wait "$SRV" 2>/dev/null || true; }
  rm -rf "$DD"
}
trap cleanup EXIT

# --- 1. stage (k3s layout: agent/containerd holds the runtime bundle,
#     agent/etc/cni holds CNI conf, the cni-plugins bundle lands beside) ---
mkdir -p "$RUNTIME_DIR" "$ETC_DIR" "$ETC_DIR/cni/net.d" "$RUN_DIR"
for f in containerd containerd-shim containerd-shim-runc-v1 containerd-shim-runc-v2 ctr runc; do
  [[ -f "$VENDOR/$f" ]] && cp "$VENDOR/$f" "$RUNTIME_DIR/$f"
done
cp -r "$VENDOR/aux" "$RUNTIME_DIR/aux" 2>/dev/null || true
chmod +x "$RUNTIME_DIR"/*
# Minimal CNI conf (k3s uses flannel; the spike only proves the wiring).
cat > "$ETC_DIR/cni/net.d/10-spike.conflist" <<'CNI'
{"cniVersion":"1.0.0","name":"spike","plugins":[{"type":"loopback"}]}
CNI
ok "staged $(ls "$RUNTIME_DIR" | tr '\n' ' ')"

# --- 2. k3s-style minimal config (CRI on the same socket, runc runtime) ---
cat > "$ETC_DIR/config.toml" <<TOML
version = 2
[plugins."io.containerd.grpc.v1.cri"]
  sandbox_image = "registry.k8s.io/pause:3.10"
[plugins."io.containerd.grpc.v1.cri".cni]
  conf_dir = "@@ETC@@/cni/net.d"
  bin_dir = "@@RUNTIME@@/aux"
[plugins."io.containerd.grpc.v1.cri".containerd]
  snapshotter = "overlayfs"
  default_runtime_name = "runc"
  [plugins."io.containerd.grpc.v1.cri".containerd.runtimes.runc]
    runtime_type = "io.containerd.runc.v2"
TOML
sed -i "s|@@ETC@@|$ETC_DIR|g; s|@@RUNTIME@@|$RUNTIME_DIR|g" "$ETC_DIR/config.toml"
ok "wrote $ETC_DIR/config.toml"

# --- 3. boot THROUGH the multicall seam + supervise ---
# Q1 deployment model: peers are reached via argv[0] symlinks, like k3s.
export INIT_PRO_DATA_DIR="$DD"
mkdir -p "$DD/bin"
ln -sf "$BIN" "$DD/bin/containerd"
ln -sf "$BIN" "$DD/bin/ctr"
CTRD="$DD/bin/containerd"
CTR="$DD/bin/ctr"
"$CTRD" \
  -c "$ETC_DIR/config.toml" \
  -a "$SOCK" \
  --root "$RUNTIME_DIR/root" \
  --state "$RUNTIME_DIR/state" \
  --log-level warn >/tmp/t41-containerd.log 2>&1 &
SRV=$!
for _ in $(seq 1 100); do [[ -S "$SOCK" ]] && break; sleep 0.1; done
if [[ -S "$SOCK" ]]; then ok "containerd up via 'init-pro containerd' (socket $SOCK)"; else
  bad "containerd socket never appeared (see /tmp/t41-containerd.log)"; cat /tmp/t41-containerd.log >&2
  echo "spike: $pass passed, $fail failed"; exit 1; fi

# --- 4. verify ---
V="$("$CTR" --address "$SOCK" version 2>&1 || true)"
if grep -q "Server:" <<<"$V" && grep -q "1.7.20" <<<"$V"; then ok "ctr version via seam: $(grep -m1 'Server:' <<<"$V")"; else bad "ctr version: $V"; fi

PLUGINS="$("$CTR" --address "$SOCK" plugins ls 2>&1 || true)"
# ctr prints type + ID + platforms + STATUS in whitespace-padded columns.
if awk '$2=="cri" && $4=="ok"' <<<"$PLUGINS" | grep -q cri; then ok "CRI plugin registered and ready"; else bad "CRI plugin missing: $(grep -c containerd <<<"$PLUGINS") plugins"; fi

# Runtime smoke (best-effort: needs a reachable registry).
if timeout 60 "$CTR" --address "$SOCK" images pull docker.io/library/busybox:1.36 >/dev/null 2>&1; then
  "$CTR" --address "$SOCK" run -d --null-io docker.io/library/busybox:1.36 t41-peek sh -c 'echo alive >/alive && sleep 300' >/dev/null 2>&1 || true
  if "$CTR" --address "$SOCK" tasks ls 2>/dev/null | grep -q t41-peek; then ok "runc task runs (busybox sandbox)"; else bad "runc task did not start"; fi
  "$CTR" --address "$SOCK" tasks kill t41-peek 2>/dev/null || true
  "$CTR" --address "$SOCK" containers delete t41-peek 2>/dev/null || true
else
  echo "SKIP runc task smoke (registry unreachable)"; fi

echo "spike: $pass passed, $fail failed"
[[ "$fail" -eq 0 ]]
