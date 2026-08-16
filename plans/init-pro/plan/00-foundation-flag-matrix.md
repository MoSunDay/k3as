# Layer 0 — Foundation: k3s flag → init-pro v1 behavior matrix

This file freezes the **`init-pro server --help` / `init-pro agent --help`
diff baseline**. It is the exhaustive categorization of every k3s `server`
and `agent` flag into one of three v1 behaviors (decision **Q9**):

| Category | Meaning |
|---|---|
| **accept-wired** | Parsed and honored by init-pro Phase-1. |
| **accept-no-op-warn** | Parsed, then logged **once per process** at WARN: *"flag `<f>` accepted but not yet implemented; no-op"*. Keeps k3s scripts working (Q2). |
| **fatal** | Rejected with a clear error — only when the value contradicts Phase-1 behavior or trips a ported k3s conflict rule. |

**Scope legend:** `S` = `server`, `A` = `agent`, `S+A` = both (flag var
reused). Source = `pkg/cli/cmds/{server,agent,stage,root,log,config}.go` and
`pkg/cli/server/server.go` (conflicts).

**Enforcement:** `scripts/cli-flag-parity-test.sh` (specified in T0.4)
asserts (1) every flag below is accepted without an "unknown flag" error
when given a type-correct value, (2) each accept-wired flag is honored
(e.g. `--data-dir` changes the resolved dir), (3) each fatal rule below
exits non-zero with the k3s-parity message, and (4) accept-no-op-warn flags
emit the deduped WARN and exit zero.

---

## Table A — `accept-wired` (Phase-1 subset)

These are the flags init-pro parses, honors, and integrates into Phase-1
behavior. All others degrade to no-op-warn or fatal.

| Flag | Scope | Type | init-pro v1 wiring | k3s source |
|---|---|---|---|---|
| `--data-dir`, `-d` | S+A | string | Sets state root (`<data-dir>`); default per platform. Stage path base (Q6). | `server.go:122`, `agent.go:291` |
| `--debug` | S+A | bool | Enables `tracing` DEBUG (k3s parity, `--debug`); env `INIT_PRO_DEBUG`. | `root.go:14` |
| `--config`, `-c` | S+A | string | Pre-clap config-file pre-scan (Q8); env `INIT_PRO_CONFIG_FILE`; default `<data-dir>/config.yaml`. | `config.go:11` |
| `--disable` | S | string-slice | Disables packaged components; validated against `DisableItems` (see below). | `server.go:523` |
| `--disable-etcd` | S | bool | Marks etcd disabled (Phase-1: recorded; etcd runtime = T2.1). Conflicts with `--datastore-endpoint` (Table B). | `server.go:565` |
| `--disable-apiserver` | S | bool | Marks apiserver disabled. Conflicts with `--datastore-endpoint` (Table B). | `server.go:553` |
| `--disable-agent` | S | bool | Disables local kubelet run. | `server.go:646` |
| `--disable-controller-manager` | S | bool | Disables controller-manager. | `server.go:559` |
| `--disable-scheduler` | S | bool | Disables scheduler. | `server.go:528` |
| `--disable-cloud-controller` | S | bool | Disables k3s CCM. | `server.go:533` |
| `--disable-kube-proxy` | S | bool | Disables kube-proxy. | `server.go:538` |
| `--disable-network-policy` | S | bool | Disables network-policy controller. | `server.go:543` |
| `--disable-helm-controller` | S | bool | Disables helm controller. | `server.go:548` |
| `--datastore-endpoint` | S | string | External datastore DSN (recorded; storage backend wiring = T2.x). Conflicts with `--disable-{apiserver,etcd}` (Table B). env `INIT_PRO_DATASTORE_ENDPOINT`. | `server.go:358` |
| `--prefer-bundled-bin` | S+A | bool | Flips child `PATH` ordering: `bin/aux` ahead of host PATH (Q6 stage). | `root.go:20` |
| `--token`, `-t` | S+A | string | Cluster join secret (recorded; auth = T1.3/T3.3). env `INIT_PRO_TOKEN`. | `server.go:129`, `agent.go:69` |
| `--server`, `-s` | S+A | string | Server URL to join (recorded; agent/client wiring = T4.5). env `INIT_PRO_URL`. | `server.go:323`, `agent.go:283` |
| `--cluster-init` | S | bool | Bootstrap cluster (recorded; etcd init = T2.1/T3.4). env `INIT_PRO_CLUSTER_INIT`. | `server.go:330` |
| `--kube-scheduler-arg` | S | string-slice | Scheduler arg passthrough (`KEY=VALUE`, repeatable). `config=<path>` loads a KubeSchedulerConfiguration JSON whose `extenders` wire the HTTP extender seam (T3.2, Q23); other keys warn no-op. | `server.go:173` |

**`--disable` valid tokens (`DisableItems`, `pkg/cli/cmds/stage.go:9`):**
`coredns`, `servicelb`, `traefik`, `local-storage`, `metrics-server`,
`runtimes`. An unknown token → fatal *"unknown disable item `<x>`"*.
(Component manifests/controllers are T6.x; v1 validates the set and records
the selection.)

---

## Table B — `fatal` (conflict rules, ported verbatim from k3s)

These exit non-zero with the k3s-parity message. Sourced from
`pkg/cli/server/server.go`.

| Rule | Trigger → message | Source |
|---|---|---|
| cluster-reset-restore-path needs cluster-reset | `--cluster-reset-restore-path` without `--cluster-reset` → *"invalid flag use; --cluster-reset required with --cluster-reset-restore-path"* | `server.go:245` |
| disable-apiserver ✗ datastore-endpoint | `--disable-apiserver` with `--datastore-endpoint` → *"invalid flag use; cannot use --disable-apiserver with --datastore-endpoint"* | `server.go:261` |
| disable-etcd ✗ datastore-endpoint | `--disable-etcd` with `--datastore-endpoint` → *"invalid flag use; cannot use --disable-etcd with --datastore-endpoint"* | `server.go:265` |
| disable-etcd needs server | `--disable-etcd` without `--server` → *"invalid flag use; --server is required with --disable-etcd"* | `server.go:257` |
| unknown `--disable` token | value not in `DisableItems` → *"unknown disable item `<x>`"* | `stage.go:9` |
| agent token required | agent with no client-kubelet cert and empty `--token` → *"--token is required"* | `pkg/cli/agent/agent.go:83-87` |
| agent server required | agent with empty `--server` → *"--server is required"* | `pkg/cli/agent/agent.go:83-87` |

(Additional k3s value-format validations — invalid `--cluster-cidr`,
`--service-cidr`, port range, `--cluster-dns`, `--tls-min-version`, etc. —
are **accept-no-op-warn** in v1 because their owning subsystems are not
Phase-1; they graduate to fatal/validation as their TODOs land.)

---

## Table C — `accept-no-op-warn` (all remaining k3s flags)

Exhaustive. Every flag k3s accepts that is not in Table A (wired) or tripped
by Table B (fatal) is accepted, parsed for type-correctness, and emits the
deduped WARN. Grouped by function; one row per flag.

### C.1 Logging / verbosity
| Flag | Scope | Type | k3s source |
|---|---|---|---|
| `--v` | S+A | int | `log.go:24` |
| `--vmodule` | S+A | string | `log.go:29` |
| `--log`, `-l` | S+A | string | `log.go:34` |
| `--alsologtostderr` | S+A | bool | `log.go:40` |

### C.2 Listener / TLS / SAN
| Flag | Scope | Type | k3s source |
|---|---|---|---|
| `--bind-address` | S+A | string | `agent.go:255` |
| `--https-listen-port` | S | int | `server.go:198` |
| `--supervisor-port` | S | int | `server.go:204` |
| `--apiserver-port` | S | int | `server.go:211` |
| `--apiserver-bind-address` | S | string | `server.go:218` |
| `--advertise-address` | S | string | `server.go:225` |
| `--advertise-port` | S | int | `server.go:230` |
| `--tls-san` | S | string-slice | `server.go:235` |
| `--tls-san-security` | S | bool | `server.go:240` |

### C.3 Cluster networking
| Flag | Scope | Type | k3s source |
|---|---|---|---|
| `--cluster-cidr` | S | string-slice | `server.go:136` |
| `--service-cidr` | S | string-slice | `server.go:141` |
| `--service-node-port-range` | S | string | `server.go:146` |
| `--cluster-dns` | S | string-slice | `server.go:152` |
| `--cluster-domain` | S | string | `server.go:157` |
| `--flannel-backend` | S | string | `server.go:252` |
| `--flannel-ipv6-masq` | S | bool | `server.go:258` |
| `--flannel-external-ip` | S | bool | `server.go:263` |
| `--egress-selector-mode` | S | string | `server.go:268` |
| `--servicelb-namespace` | S | string | `server.go:274` |
| `--flannel-iface` | S+A | string | `agent.go:170` |
| `--flannel-conf` | S+A | string | `agent.go:175` |
| `--flannel-cni-conf` | S+A | string | `agent.go:180` |

### C.4 Kubeconfig / client bootstrap
| Flag | Scope | Type | k3s source |
|---|---|---|---|
| `--write-kubeconfig`, `-o` | S | string | `server.go:280` |
| `--write-kubeconfig-mode` | S | string | `server.go:287` |
| `--write-kubeconfig-group` | S | string | `server.go:294` |
| `--token-file` | S+A | string | `server.go:305`, `agent.go:277` |
| `--agent-token` | S | string | `server.go:311` |
| `--agent-token-file` | S | string | `server.go:317` |

### C.5 Cluster lifecycle / snapshots
| Flag | Scope | Type | k3s source |
|---|---|---|---|
| `--cluster-reset` | S | bool | `server.go:335` |
| `--cluster-reset-restore-path` | S | string | `server.go:341` (fatal only without `--cluster-reset`, see Table B) |
| `--helm-job-image` | S | string | `server.go:300` (deprecated) |
| `--etcd-expose-metrics` | S | bool | `server.go:382` |
| `--etcd-disable-snapshots` | S | bool | `server.go:387` |
| `--etcd-snapshot-name` | S | string | `server.go:392` |
| `--etcd-snapshot-schedule-cron` | S | string | `server.go:398` |
| `--etcd-snapshot-reconcile-interval` | S | duration | `server.go:404` |
| `--etcd-snapshot-retention` | S | int | `server.go:410` |
| `--etcd-snapshot-dir` | S | string | `server.go:416` |
| `--etcd-snapshot-compress` | S | bool | `server.go:421` |
| `--etcd-s3` | S | bool | `server.go:426` |
| `--etcd-s3-endpoint` | S | string | `server.go:431` |
| `--etcd-s3-endpoint-ca` | S | string | `server.go:437` |
| `--etcd-s3-skip-ssl-verify` | S | bool | `server.go:442` |
| `--etcd-s3-access-key` | S | string | `server.go:447` |
| `--etcd-s3-secret-key` | S | string | `server.go:453` |
| `--etcd-s3-session-token` | S | string | `server.go:459` |
| `--etcd-s3-bucket` | S | string | `server.go:465` |
| `--etcd-s3-bucket-lookup-type` | S | string | `server.go:470` |
| `--etcd-s3-region` | S | string | `server.go:476` |
| `--etcd-s3-folder` | S | string | `server.go:482` |
| `--etcd-s3-retention` | S | int | `server.go:487` |
| `--etcd-s3-proxy` | S | string | `server.go:497` |
| `--etcd-s3-config-secret` | S | string | `server.go:502` |
| `--etcd-s3-insecure` | S | bool | `server.go:507` |
| `--etcd-s3-timeout` | S | duration | `server.go:512` |

### C.6 Datastore extras / kine
| Flag | Scope | Type | k3s source |
|---|---|---|---|
| `--kine-tls` | S | bool | `server.go:353` |
| `--datastore-cafile` | S | string | `server.go:364` |
| `--datastore-certfile` | S | string | `server.go:370` |
| `--datastore-keyfile` | S | string | `server.go:376` |

### C.7 Component arg passthrough
| Flag | Scope | Type | k3s source |
|---|---|---|---|
| `--kube-apiserver-arg` | S | string-slice | `server.go:163` |
| `--etcd-arg` | S | string-slice | `server.go:168` |
| `--kube-controller-manager-arg` | S | string-slice | `server.go:178` |
| `--kube-controller-arg` | S | string-slice | `server.go:652` (hidden alias) |
| `--kube-cloud-controller-manager-arg` | S | string-slice | `server.go:347` |
| `--kube-cloud-controller-arg` | S | string-slice | `server.go:658` (hidden alias) |
| `--helm-controller-arg` | S | string-slice | `server.go:183` |
| `--kubelet-arg` | S+A | string-slice | `agent.go:203` |
| `--kube-proxy-arg` | S+A | string-slice | `agent.go:208` |

### C.8 Storage / registry / images
| Flag | Scope | Type | k3s source |
|---|---|---|---|
| `--default-local-storage-path` | S | string | `server.go:518` |
| `--embedded-registry` | S | bool | `server.go:571` |
| `--supervisor-metrics` | S | bool | `server.go:577` |
| `--system-default-registry` | S | string | `server.go:597` |
| `--pause-image` | S+A | string | `agent.go:158` |
| `--private-registry` | S+A | string | `agent.go:146` |
| `--disable-default-registry-endpoint` | S+A | bool | `agent.go:240` |
| `--airgap-extra-registry` | S+A | string-slice | `agent.go:152` |

### C.9 Node identity / kubelet
| Flag | Scope | Type | k3s source |
|---|---|---|---|
| `--node-name` | S+A | string | `agent.go:97` |
| `--with-node-id` | S+A | bool | `agent.go:103` |
| `--node-label` | S+A | string-slice | `agent.go:218` |
| `--node-taint` | S+A | string-slice | `agent.go:213` |
| `--node-ip`, `-i` | S+A | string-slice | `agent.go:76` |
| `--node-external-ip` | S+A | string-slice | `agent.go:82` |
| `--node-internal-dns` | S+A | string-slice | `agent.go:87` |
| `--node-external-dns` | S+A | string-slice | `agent.go:92` |
| `--resolv-conf` | S+A | string | `agent.go:197` |
| `--protect-kernel-defaults` | S+A | bool | `agent.go:108` |

### C.10 Container runtime
| Flag | Scope | Type | k3s source |
|---|---|---|---|
| `--container-runtime-endpoint` | S+A | string | `agent.go:131` |
| `--default-runtime` | S+A | string | `agent.go:136` |
| `--image-service-endpoint` | S+A | string | `agent.go:141` |
| `--snapshotter` | S+A | string | `agent.go:164` |
| `--image-credential-provider-bin-dir` | S+A | string | `agent.go:223` |
| `--image-credential-provider-config` | S+A | string | `agent.go:229` |
| `--nonroot-devices` | S+A | bool | `agent.go:245` |
| `--docker` | S+A | bool | `agent.go:126` (experimental/deprecated) |
| `--disable-apiserver-lb` | A | bool | `agent.go:235` (experimental) |

### C.11 Security / misc / experimental
| Flag | Scope | Type | k8s source |
|---|---|---|---|
| `--secrets-encryption` | S | bool | `server.go:625` |
| `--secrets-encryption-provider` | S | string | `server.go:636` |
| `--rootless` | S+A | bool | `server.go:630`, `agent.go:330` (experimental) |
| `--selinux` | S+A | bool | `agent.go:113` |
| `--lb-server-port` | S+A | int | `agent.go:119` |
| `--enable-pprof` | S+A | bool | `agent.go:250` (experimental) |
| `--vpn-auth` | S+A | string | `agent.go:185` (experimental) |
| `--vpn-auth-file` | S+A | string | `agent.go:191` (experimental) |

---

## Env-var parity (k3s → init-pro)

init-pro uses an `INIT_PRO_*` prefix in place of k3s's `K3S_*`. The wired
flags' env equivalents: `INIT_PRO_DEBUG`, `INIT_PRO_CONFIG_FILE`,
`INIT_PRO_DATA_DIR`, `INIT_PRO_TOKEN`, `INIT_PRO_URL`,
`INIT_PRO_CLUSTER_INIT`, `INIT_PRO_DATASTORE_ENDPOINT`,
`INIT_PRO_LB_SERVER_PORT`, `INIT_PRO_RESOLV_CONF`, `INIT_PRO_SELINUX`,
`INIT_PRO_NODE_NAME`. Unknown `K3S_*` env vars are ignored in v1 (no
inheritance) to avoid silent cross-wiring; the parity test documents this.

---

## Summary counts

- **accept-wired:** 17 flag entries (the Phase-1 subset above).
- **accept-no-op-warn:** ~113 flag entries (Tables C.1–C.11).
- **fatal:** 7 conflict rules (Table B).
- **Total k3s surface covered:** ~130 server+agent flag entries (exhaustive
  per the `pkg/cli/cmds/*.go` audit).

---

## Change policy

This matrix is the **frozen v1 baseline**. Moving a flag from
accept-no-op-warn to accept-wired requires: (1) its owning TODO landing,
(2) updating this table, (3) updating `scripts/cli-flag-parity-test.sh`,
(4) re-freezing. No flag may move from accept-wired back to no-op-warn
without a Q9 re-evaluation.
