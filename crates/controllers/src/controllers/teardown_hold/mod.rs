//! Recreate runtime-teardown hold (INF4, TODO **T7.6**, decision **Q62**):
//! pure predicates + verdict math for the Deployment controller's safe
//! Recreate gate. The wiring (live node GETs, annotation persistence) lives
//! in [`super::deployment`]; everything here is side-effect free.
//!
//! Upstream Recreate trusts RS spec/status zero + Pod deletion as the "old
//! runtime is gone" signal. For T7.6 inference Pods (Q60 host-direct rootfs
//! + Q61 whole-GPU allocation) that is NOT proof: node-side teardown
//! (cgroup cleanup, device release, ledger entry retirement) can still be
//! in flight after the Pod object disappears, and a NEW host-direct GPU Pod
//! racing the old one would contend for the same exclusive device. The
//! Deployment controller therefore holds the new RS at 0 replicas until the
//! runtime teardown of every old host-direct/GPU Pod is explicitly
//! acknowledged by the nodes that hosted them:
//!
//! * confirmation per old Pod = (a) Pod object gone AND (b) the CURRENT
//!   node object's `status.initProRuntime.released` carries an entry with
//!   that podUID whose `at` timestamp is STRICTLY NEWER than the rollout's
//!   teardown-start wall time AND (c) NO node's
//!   `status.initProRuntime.gpuLedger.allocations` still references the
//!   podUID;
//! * the teardown-start wall time lives on the Deployment as the
//!   `init-pro.io/teardown-since` annotation (RFC3339, set ONCE when the
//!   hold begins, cleared on completion) — a stale ack is either pruned by
//!   the node (~10 min) or older than teardown-since, so it can never
//!   unblock the rollout;
//! * the watched podUID/node set survives Pod deletion on the
//!   `init-pro.io/teardown-pending` annotation, and every check reads
//!   fresh node objects (live GETs of hosting nodes + a live LIST for the
//!   ledger sweep), never cached events.
//!
//! Ordinary Pods (no host-direct annotation, no GPU request) keep the
//! upstream fast path untouched; the Q58 canary machinery is
//! RollingUpdate-based and never enters this branch.

use serde_json::Value;

use super::owned_by;

/// Deployment annotation: RFC3339 wall time the teardown hold began
/// (INF4). Acks strictly older than this never unblock the rollout.
pub(crate) const TEARDOWN_SINCE_ANNOTATION: &str = "init-pro.io/teardown-since";
/// Deployment annotation: JSON `[{"podUID": "...", "node": "..."}]` of old
/// host-direct/GPU pods whose runtime teardown the hold waits for. The Pod
/// objects may already be gone; this is the correlation key.
pub(crate) const TEARDOWN_PENDING_ANNOTATION: &str = "init-pro.io/teardown-pending";

/// Default kubelet node-lease heartbeat period (INF4, T7.6, Q62, S47
/// review F3): mirror of the kubelet runner default; the controller
/// reads node objects, not kubelet config, so the default is the
/// planning basis.
const NODE_HEARTBEAT_DEFAULT_SECS: u64 = 10;

/// Clock-skew safety margin for release-ack confirmation (INF4, T7.6,
/// Q62, S47 review F3): an ack counts only when
/// `released.at > teardown-since + margin`. A node clock running ahead
/// by up to the margin can no longer unblock the hold with a stale ack;
/// a node clock running behind merely delays confirmation by the margin
/// (bounded, never deadlocks). Field assumption: control-plane vs node
/// NTP skew <= margin (plans/init-pro/ops/node04-llama-migration.md).
const CONFIRM_MARGIN_SECS: u64 = if NODE_HEARTBEAT_DEFAULT_SECS * 2 > 60 {
    NODE_HEARTBEAT_DEFAULT_SECS * 2
} else {
    60
};

/// The one literal annotation value selecting host-direct (Q60 contract,
/// mirrored from kubelet `engine::host_direct`).
const ROOTFS_ANNOTATION: &str = "init-pro.io/rootfs";
const HOST_DIRECT_ROOT: &str = "/";

/// One old Pod the Recreate hold waits for (INF4): correlation podUID (the
/// key the node's `released`/`gpuLedger` records use) + the hosting node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TeardownWatch {
    pub pod_uid: String,
    pub node: String,
}

/// True when the Pod was host-direct (literal `init-pro.io/rootfs: /`,
/// Q60). Mirrors kubelet `host_direct::host_direct_requested` — controllers
/// cannot depend on the kubelet crate, so the contract is mirrored here
/// against the same literals.
fn host_direct_requested(pod: &Value) -> bool {
    pod.pointer("/metadata/annotations")
        .and_then(Value::as_object)
        .and_then(|m| m.get(ROOTFS_ANNOTATION))
        .and_then(Value::as_str)
        == Some(HOST_DIRECT_ROOT)
}

/// True when any container (or initContainer) requests at least one whole
/// GPU (Q61, `common::gpu` vendor resources). Mirrors kubelet
/// `host_direct::pod_uses_gpu`.
fn gpu_requested(pod: &Value) -> bool {
    for path in ["/spec/containers", "/spec/initContainers"] {
        let Some(list) = pod.pointer(path).and_then(Value::as_array) else {
            continue;
        };
        for c in list {
            let Some(reqs) = c.pointer("/resources/requests").and_then(Value::as_object) else {
                continue;
            };
            for (res, qty) in reqs {
                if !common::gpu::is_gpu_resource(res) {
                    continue;
                }
                let n = match qty {
                    Value::Number(n) => n.as_f64().unwrap_or(0.0),
                    Value::String(s) => s.parse::<f64>().unwrap_or(0.0),
                    _ => 0.0,
                };
                if n > 0.0 {
                    return true;
                }
            }
        }
    }
    false
}

/// True when the Pod's runtime teardown needs explicit node confirmation
/// (INF4): host-direct rootfs or whole-GPU requests. Ordinary Pods keep
/// the upstream Recreate fast path (Pod deletion is proof enough).
pub(crate) fn needs_runtime_teardown(pod: &Value) -> bool {
    host_direct_requested(pod) || gpu_requested(pod)
}

/// Correlation key for one Pod: `metadata.uid`, falling back to the name
/// (defensive — v1 storage fills uids on create, but objects written
/// directly to storage may lack one).
fn pod_correlation_id(pod: &Value) -> String {
    let uid = pod
        .pointer("/metadata/uid")
        .and_then(Value::as_str)
        .unwrap_or("");
    if uid.is_empty() {
        pod.pointer("/metadata/name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    } else {
        uid.to_string()
    }
}

/// Live watch set (INF4): `pods` still present and owned by one of `olds`
/// (controller-ref — `owned_by` already falls back to kind+name when the
/// RS carries no uid) that need runtime teardown. Sorted by podUID for
/// deterministic annotations and messages.
pub(crate) fn teardown_watch_from_pods(olds: &[Value], pods: &[Value]) -> Vec<TeardownWatch> {
    let mut watch: Vec<TeardownWatch> = pods
        .iter()
        .filter(|p| {
            olds.iter().any(|rs| {
                owned_by(
                    p,
                    "ReplicaSet",
                    crate::object::name(rs).unwrap_or_default(),
                    rs.pointer("/metadata/uid")
                        .and_then(Value::as_str)
                        .unwrap_or(""),
                )
            })
        })
        .filter(|p| needs_runtime_teardown(p))
        .map(|p| TeardownWatch {
            pod_uid: pod_correlation_id(p),
            node: p
                .pointer("/spec/nodeName")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        })
        .collect();
    watch.sort_by(|a, b| a.pod_uid.cmp(&b.pod_uid));
    watch.dedup_by(|a, b| a.pod_uid == b.pod_uid);
    watch
}

/// Parse the `init-pro.io/teardown-pending` annotation back into the watch
/// set (garbage-tolerant: unparseable entries are dropped, never fatal).
pub(crate) fn teardown_watch_from_annotation(dep: &Value) -> Vec<TeardownWatch> {
    let Some(raw) = dep
        .pointer("/metadata/annotations")
        .and_then(|a| a.get(TEARDOWN_PENDING_ANNOTATION))
        .and_then(Value::as_str)
    else {
        return Vec::new();
    };
    let Ok(Value::Array(items)) = serde_json::from_str::<Value>(raw) else {
        return Vec::new();
    };
    let mut watch: Vec<TeardownWatch> = items
        .iter()
        .filter_map(|i| {
            Some(TeardownWatch {
                pod_uid: i.get("podUID")?.as_str()?.to_string(),
                node: i.get("node")?.as_str()?.to_string(),
            })
        })
        .filter(|w| !w.pod_uid.is_empty())
        .collect();
    watch.sort_by(|a, b| a.pod_uid.cmp(&b.pod_uid));
    watch.dedup_by(|a, b| a.pod_uid == b.pod_uid);
    watch
}

/// Serialize the watch set for the `init-pro.io/teardown-pending`
/// annotation (canonical, sorted input expected from the callers above).
pub(crate) fn teardown_watch_to_annotation(watch: &[TeardownWatch]) -> String {
    Value::Array(
        watch
            .iter()
            .map(|w| serde_json::json!({"podUID": w.pod_uid, "node": w.node}))
            .collect(),
    )
    .to_string()
}

/// Lenient RFC3339 -> unix seconds for node-posted timestamps (INF4): the
/// strict `common::time::parse_rfc3339` rejects fractional seconds and
/// offsets, and a node-side format drift must never deadlock the hold —
/// it would make every ack unparseable forever. Accepts the canonical
/// fixed-width form plus an optional `.frac` tail and `Z`/`±HH:MM`.
fn parse_rfc3339_lenient(s: &str) -> Option<u64> {
    let head = s.get(..19)?;
    let secs = crate::time::parse_rfc3339(&format!("{head}Z"))?;
    let tail = &s[19..];
    let (frac, tz) = match tail.find(['Z', 'z', '+', '-']) {
        Some(i) => (&tail[..i], &tail[i..]),
        None => ("", ""),
    };
    if !frac.is_empty() && !frac.starts_with('.') {
        return None;
    }
    if frac.len() > 1 && frac[1..].chars().any(|c| !c.is_ascii_digit()) {
        return None;
    }
    // Sub-second fractions are below the hold's second-resolution compare.
    match tz {
        "" | "Z" | "z" => Some(secs),
        _ => {
            let sign: i64 = if tz.starts_with('-') { -1 } else { 1 };
            let rest = tz.trim_start_matches(['+', '-']);
            let (h, m) = rest.split_once(':')?;
            let off = h.parse::<i64>().ok()? * 3600 + m.parse::<i64>().ok()? * 60;
            Some((secs as i64 - sign * off).max(0) as u64)
        }
    }
}

/// (b): the node's CURRENT `status.initProRuntime.released` carries an
/// entry for `pod_uid` whose `at` is strictly newer than
/// `since_secs + CONFIRM_MARGIN_SECS` - fast-clock defeat, S47 review
/// F3 (INF4 stale-ack defeat: equal-or-older acks, and acks a fast node
/// clock could fake within the margin, predate the hold).
pub(crate) fn released_after(node: &Value, pod_uid: &str, since_secs: u64) -> bool {
    node.pointer("/status/initProRuntime/released")
        .and_then(Value::as_array)
        .map(|entries| {
            entries.iter().any(|e| {
                e.get("podUID").and_then(Value::as_str) == Some(pod_uid)
                    && e.get("at")
                        .and_then(Value::as_str)
                        .and_then(parse_rfc3339_lenient)
                        .map(|at| at > since_secs.saturating_add(CONFIRM_MARGIN_SECS))
                        .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

/// Latest observed `at` for `pod_uid` in the node's released records
/// (INF4 hold-message enrichment, S47 review F3): scans
/// `status.initProRuntime.released`, matches by podUID, picks the max
/// `at` by [`parse_rfc3339_lenient`], and falls back to the LAST
/// matching raw string when entries are unparseable (node-side format
/// drift must never blank the observed timestamp).
pub(crate) fn latest_released_at<'a>(node: &'a Value, pod_uid: &str) -> Option<&'a str> {
    node.pointer("/status/initProRuntime/released")
        .and_then(Value::as_array)?
        .iter()
        .filter(|e| e.get("podUID").and_then(Value::as_str) == Some(pod_uid))
        .filter_map(|e| {
            let raw = e.get("at").and_then(Value::as_str)?;
            Some((parse_rfc3339_lenient(raw).unwrap_or(0), raw))
        })
        .max_by_key(|(secs, _)| *secs)
        .map(|(_, raw)| raw)
}

/// (c): any node's `status.initProRuntime.gpuLedger` still references
/// `pod_uid` (live allocation — teardown not complete). The ledger is
/// keyed by vendor resource (`nvidia.com/gpu`, `amd.com/gpu`), each
/// carrying its own `allocations` map keyed by podUID (Q61 shape); every
/// resource entry is swept, so a podUID under ANY vendor counts.
pub(crate) fn ledger_references(nodes: &[Value], pod_uid: &str) -> bool {
    nodes.iter().any(|n| {
        n.pointer("/status/initProRuntime/gpuLedger")
            .and_then(Value::as_object)
            .map(|ledger| {
                ledger.values().any(|entry| {
                    entry
                        .get("allocations")
                        .and_then(Value::as_object)
                        .map(|alloc| alloc.contains_key(pod_uid))
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false)
    })
}

/// Verdict of the pure Recreate-hold evaluation (INF4). The caller turns
/// `Begin`/`Waiting` into "new RS pinned at 0" + annotations, and
/// `Complete` into annotation removal + proceed.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RecreateHold {
    /// No teardown-relevant old pods and no active hold: the upstream
    /// fast path (zero extra reads, zero regression).
    FastPath,
    /// Hold begins now: record teardown-since (+ the watch set) and pin
    /// the new RS at 0. Returned even when the annotation is already set
    /// but unparseable (self-healing re-record).
    Begin(Vec<TeardownWatch>),
    /// Active hold; these entries are unconfirmed, each with its
    /// actionable reason (`pod-terminating` / `release-unacked` /
    /// `ledger-referenced`).
    Waiting(Vec<(TeardownWatch, &'static str)>),
    /// Every watched teardown confirmed: clear the annotations, proceed
    /// with the new-RS scale-up.
    Complete,
}

/// Pure hold evaluation (INF4): `pending` is the annotation watch set,
/// `live` the freshly listed old-RS pods needing teardown (live entries
/// win placement ties), `hosting_nodes` FRESH GETs of the distinct
/// hosting node objects, `all_nodes` a fresh full node list for the
/// ledger sweep, `since` the current teardown-since annotation.
pub(crate) fn evaluate_recreate_hold(
    pending: &[TeardownWatch],
    live: &[TeardownWatch],
    hosting_nodes: &[Value],
    all_nodes: &[Value],
    since: Option<&str>,
) -> RecreateHold {
    // Union, pending first so a live entry replaces its placement.
    let mut watch = pending.to_vec();
    for l in live {
        if let Some(w) = watch.iter_mut().find(|w| w.pod_uid == l.pod_uid) {
            *w = l.clone();
        } else {
            watch.push(l.clone());
        }
    }
    if watch.is_empty() {
        // Nothing to wait for. A leftover teardown-since with an empty
        // watch (annotation wiped mid-hold) still completes + clears.
        return if since.is_some() {
            RecreateHold::Complete
        } else {
            RecreateHold::FastPath
        };
    }
    let Some(since_str) = since else {
        return RecreateHold::Begin(watch);
    };
    let Some(since_secs) = parse_rfc3339_lenient(since_str) else {
        return RecreateHold::Begin(watch);
    };
    let mut unconfirmed = Vec::new();
    for w in &watch {
        let pod_gone = !live.iter().any(|l| l.pod_uid == w.pod_uid);
        let acked = hosting_nodes
            .iter()
            .find(|n| crate::object::name(n) == Some(w.node.as_str()))
            .map(|n| released_after(n, &w.pod_uid, since_secs))
            .unwrap_or(false);
        let ledger_clean = !ledger_references(all_nodes, &w.pod_uid);
        if pod_gone && acked && ledger_clean {
            continue;
        }
        let why = if !pod_gone {
            "pod-terminating"
        } else if !acked {
            "release-unacked"
        } else {
            "ledger-referenced"
        };
        unconfirmed.push((w.clone(), why));
    }
    if unconfirmed.is_empty() {
        RecreateHold::Complete
    } else {
        RecreateHold::Waiting(unconfirmed)
    }
}

/// Actionable hold message (INF4, S47 review F3): lists every waiting
/// podUID + node with its reason, the observed latest release-ack time
/// for that podUID on its hosting node (`ack-at`, unset when the node
/// has no matching record), and closes with the teardown-since context
/// plus the confirmation margin so field ops can compare both sides'
/// clocks from the message alone.
pub(crate) fn hold_message(
    unconfirmed: &[(TeardownWatch, &'static str)],
    hosting_nodes: &[Value],
    since: Option<&str>,
) -> String {
    let entries: Vec<String> = unconfirmed
        .iter()
        .map(|(w, why)| {
            let ack_at = hosting_nodes
                .iter()
                .find(|n| crate::object::name(n) == Some(w.node.as_str()))
                .and_then(|n| latest_released_at(n, &w.pod_uid))
                .unwrap_or("unset");
            format!("{}@{} ({why}, ack-at={ack_at})", w.pod_uid, w.node)
        })
        .collect();
    format!(
        "Recreate hold: old runtime teardown pending for host-direct/GPU pods: {} (teardown-since={}, margin={CONFIRM_MARGIN_SECS}s)",
        entries.join(", "),
        since.unwrap_or("unset")
    )
}

#[cfg(test)]
mod tests;
