//! INF4 (T7.6, Q62) unit tests for the pure Recreate runtime-teardown
//! hold: predicate selection (host-direct `/` + whole-GPU requests), watch
//! set build/roundtrip, the lenient RFC3339 ack parser, and the
//! Begin/Waiting/Complete verdict matrix incl. the stale-ack and
//! live-ledger defeats.

use super::*;
use serde_json::json;

fn teardown_pod(name_: &str, uid: &str, node: &str, host_direct: bool, gpu: bool) -> Value {
    let mut annotations = serde_json::Map::new();
    if host_direct {
        annotations.insert("init-pro.io/rootfs".into(), json!("/"));
    }
    let request = if gpu {
        json!({"requests": {"nvidia.com/gpu": 1}})
    } else {
        json!({"requests": {"cpu": 1}})
    };
    json!({
        "apiVersion": "v1", "kind": "Pod",
        "metadata": {"name": name_, "namespace": "default", "uid": uid,
                     "annotations": annotations,
                     "ownerReferences": [
                        {"kind": "ReplicaSet", "name": "dep-old",
                         "uid": "rs-old-uid", "controller": true}]},
        "spec": {"nodeName": node, "containers": [
            {"name": "c", "image": "ignored", "resources": request}]},
    })
}

fn node_with_runtime(name_: &str, released: Value, allocations: Value) -> Value {
    json!({
        "apiVersion": "v1", "kind": "Node",
        "metadata": {"name": name_},
        "status": {"initProRuntime": {
            "gpuLedger": {"nvidia.com/gpu": {"total": 1, "allocations": allocations}},
            "released": released,
        }},
    })
}

#[test]
fn needs_runtime_teardown_matches_host_direct_and_gpu() {
    // Host-direct, GPU, both, and the ordinary fast path.
    assert!(needs_runtime_teardown(&teardown_pod(
        "a", "u-a", "n1", true, false
    )));
    assert!(needs_runtime_teardown(&teardown_pod(
        "b", "u-b", "n1", false, true
    )));
    assert!(needs_runtime_teardown(&teardown_pod(
        "c", "u-c", "n1", true, true
    )));
    assert!(!needs_runtime_teardown(&teardown_pod(
        "d", "u-d", "n1", false, false
    )));
    // Non-literal rootfs annotation (`host`) is NOT host-direct (Q60).
    let mut pod = teardown_pod("e", "u-e", "n1", false, false);
    pod["metadata"]["annotations"]["init-pro.io/rootfs"] = json!("host");
    assert!(!needs_runtime_teardown(&pod));
    // AMD vendor resource counts (common::gpu, Q61).
    let mut amd = teardown_pod("f", "u-f", "n1", false, false);
    amd["spec"]["containers"][0]["resources"]["requests"]["amd.com/gpu"] = json!(1);
    assert!(needs_runtime_teardown(&amd));
    // initContainers GPU requests count too.
    let mut init = teardown_pod("g", "u-g", "n1", false, false);
    init["spec"]["initContainers"] = json!([{
            "name": "i", "resources": {"requests": {"nvidia.com/gpu": 1}}}]);
    assert!(needs_runtime_teardown(&init));
}

#[test]
fn watch_from_pods_selects_owned_teardown_pods_only() {
    let old_rs = json!({"metadata": {"name": "dep-old", "uid": "rs-old-uid"}});
    let pods = vec![
        teardown_pod("gpu", "u-1", "node-a", false, true),
        teardown_pod("plain", "u-2", "node-a", false, false),
        teardown_pod("hd", "u-3", "node-b", true, false),
    ];
    let watch = teardown_watch_from_pods(std::slice::from_ref(&old_rs), &pods);
    assert_eq!(
        watch,
        vec![
            TeardownWatch {
                pod_uid: "u-1".into(),
                node: "node-a".into()
            },
            TeardownWatch {
                pod_uid: "u-3".into(),
                node: "node-b".into()
            },
        ]
    );
    // A pod owned by a DIFFERENT RS is not watched (both name and uid
    // differ — `owned_by` matches either precisely).
    let mut foreign = teardown_pod("x", "u-9", "node-a", true, false);
    foreign["metadata"]["ownerReferences"][0]["name"] = json!("other-rs");
    foreign["metadata"]["ownerReferences"][0]["uid"] = json!("other-uid");
    assert!(teardown_watch_from_pods(std::slice::from_ref(&old_rs), &[foreign]).is_empty());
}

#[test]
fn watch_annotation_roundtrip_and_garbage_tolerance() {
    let watch = vec![
        TeardownWatch {
            pod_uid: "u-1".into(),
            node: "n1".into(),
        },
        TeardownWatch {
            pod_uid: "u-2".into(),
            node: "n2".into(),
        },
    ];
    let dep = json!({"metadata": {"annotations": {
            TEARDOWN_PENDING_ANNOTATION: teardown_watch_to_annotation(&watch)}}});
    assert_eq!(teardown_watch_from_annotation(&dep), watch);
    // Garbage never panics and never resurrects entries.
    let bad = json!({"metadata": {"annotations": {
            TEARDOWN_PENDING_ANNOTATION: "not json"}}});
    assert!(teardown_watch_from_annotation(&bad).is_empty());
    assert!(teardown_watch_from_annotation(&json!({})).is_empty());
}

#[test]
fn lenient_rfc3339_accepts_fractions_and_offsets() {
    use crate::time::now_rfc3339;
    let t = 1_700_000_000u64;
    let strict = now_rfc3339(t);
    assert_eq!(parse_rfc3339_lenient(&strict), Some(t));
    let head = &strict[..19]; // strip the trailing "Z" before re-suffixing
    assert_eq!(
        parse_rfc3339_lenient(&format!("{head}.123456789Z")),
        Some(t)
    );
    assert_eq!(parse_rfc3339_lenient(&format!("{head}+00:00")), Some(t));
    // +02:00 local == t - 7200 in UTC.
    assert_eq!(
        parse_rfc3339_lenient(&format!("{head}+02:00")),
        Some(t - 7200)
    );
    assert_eq!(
        parse_rfc3339_lenient(&format!("{head}-01:30")),
        Some(t + 5400)
    );
    assert_eq!(parse_rfc3339_lenient("garbage"), None);
    assert_eq!(parse_rfc3339_lenient(""), None);
}

#[test]
fn hold_begins_when_teardown_pods_appear() {
    let live = vec![TeardownWatch {
        pod_uid: "u-1".into(),
        node: "n1".into(),
    }];
    // No annotation yet: Begin, even if the node already carries a
    // release record (an ack can never predate the hold — stale-ack
    // defeat by construction).
    let node = node_with_runtime(
        "n1",
        json!([{"podUID": "u-1", "at": "2026-01-01T00:00:00Z",
                    "reason": "teardown-complete"}]),
        json!({}),
    );
    assert_eq!(
        evaluate_recreate_hold(
            &[],
            &live,
            std::slice::from_ref(&node),
            std::slice::from_ref(&node),
            None
        ),
        RecreateHold::Begin(live.clone())
    );
    // Unparseable teardown-since self-heals into a fresh Begin.
    assert_eq!(
        evaluate_recreate_hold(
            &[],
            &live,
            std::slice::from_ref(&node),
            std::slice::from_ref(&node),
            Some("garbage")
        ),
        RecreateHold::Begin(live)
    );
}

#[test]
fn hold_waiting_reasons_and_completion() {
    let since = "2026-01-01T00:10:00Z";
    let watch = vec![TeardownWatch {
        pod_uid: "u-1".into(),
        node: "n1".into(),
    }];
    // (i) pod gone, NO release record -> held, release-unacked.
    let no_ack = node_with_runtime("n1", json!([]), json!({}));
    assert_eq!(
        evaluate_recreate_hold(
            &watch,
            &[],
            std::slice::from_ref(&no_ack),
            std::slice::from_ref(&no_ack),
            Some(since)
        ),
        RecreateHold::Waiting(vec![(watch[0].clone(), "release-unacked")])
    );
    // (ii) fresh ack + clean ledger -> Complete. The ack must clear
    // since + CONFIRM_MARGIN_SECS (60s, S47 review F3): 00:11:05 works,
    // 00:11:00 (= since + margin exactly) would NOT (strictly-greater).
    let fresh = node_with_runtime(
        "n1",
        json!([{"podUID": "u-1", "at": "2026-01-01T00:11:05Z",
                    "reason": "teardown-complete"}]),
        json!({}),
    );
    assert_eq!(
        evaluate_recreate_hold(
            &watch,
            &[],
            std::slice::from_ref(&fresh),
            std::slice::from_ref(&fresh),
            Some(since)
        ),
        RecreateHold::Complete
    );
    // (iii) stale ack (at == since, and at < since) -> still held.
    let stale_eq = node_with_runtime(
        "n1",
        json!([{"podUID": "u-1", "at": since, "reason": "teardown-complete"}]),
        json!({}),
    );
    assert_eq!(
        evaluate_recreate_hold(&watch, &[], &[stale_eq], &[], Some(since)),
        RecreateHold::Waiting(vec![(watch[0].clone(), "release-unacked")])
    );
    let stale_old = node_with_runtime(
        "n1",
        json!([{"podUID": "u-1", "at": "2026-01-01T00:09:59Z",
                    "reason": "teardown-complete"}]),
        json!({}),
    );
    assert_eq!(
        evaluate_recreate_hold(&watch, &[], &[stale_old], &[], Some(since)),
        RecreateHold::Waiting(vec![(watch[0].clone(), "release-unacked")])
    );
    // (iv) fresh ack but ANOTHER node's ledger still references the
    // podUID -> held, ledger-referenced.
    let other = node_with_runtime("n2", json!([]), json!({"u-1": {"c": [0]}}));
    let nodes = [fresh.clone(), other];
    assert_eq!(
        evaluate_recreate_hold(&watch, &[], &nodes[..1], &nodes, Some(since)),
        RecreateHold::Waiting(vec![(watch[0].clone(), "ledger-referenced")])
    );
    // Live pod still terminating -> pod-terminating reason.
    assert_eq!(
        evaluate_recreate_hold(&watch, &watch, &[fresh], &[], Some(since)),
        RecreateHold::Waiting(vec![(watch[0].clone(), "pod-terminating")])
    );
}

#[test]
fn hold_fast_path_and_empty_completion() {
    // Ordinary world: no watch, no annotation -> FastPath.
    assert_eq!(
        evaluate_recreate_hold(&[], &[], &[], &[], None),
        RecreateHold::FastPath
    );
    // Leftover teardown-since with an empty watch -> Complete (clears).
    assert_eq!(
        evaluate_recreate_hold(&[], &[], &[], &[], Some("2026-01-01T00:00:00Z")),
        RecreateHold::Complete
    );
}

#[test]
fn fast_node_clock_within_margin_cannot_confirm() {
    // S47 review F3 (skew direction 1 — node clock AHEAD): an ack at
    // since + margin - 1 would satisfy the old bare `at > since` compare
    // while the release may actually predate the hold start (the node's
    // clock stamped it into the future). The margin keeps the hold.
    let since = "2026-01-01T00:10:00Z";
    let watch = vec![TeardownWatch {
        pod_uid: "u-1".into(),
        node: "n1".into(),
    }];
    let fast = node_with_runtime(
        "n1",
        json!([{"podUID": "u-1", "at": "2026-01-01T00:10:59Z",
                    "reason": "teardown-complete"}]),
        json!({}),
    );
    assert_eq!(
        evaluate_recreate_hold(&watch, &[], &[fast], &[], Some(since)),
        RecreateHold::Waiting(vec![(watch[0].clone(), "release-unacked")])
    );
}

#[test]
fn slow_node_clock_beyond_margin_still_confirms() {
    // S47 review F3 (skew direction 2, node clock BEHIND): the margin
    // only DELAYS confirmation (bounded by margin, never deadlocks); an
    // ack beyond since + margin confirms normally.
    let since = "2026-01-01T00:10:00Z";
    let watch = vec![TeardownWatch {
        pod_uid: "u-1".into(),
        node: "n1".into(),
    }];
    let slow = node_with_runtime(
        "n1",
        json!([{"podUID": "u-1", "at": "2026-01-01T00:11:05Z",
                    "reason": "teardown-complete"}]),
        json!({}),
    );
    assert_eq!(
        evaluate_recreate_hold(
            &watch,
            &[],
            std::slice::from_ref(&slow),
            std::slice::from_ref(&slow),
            Some(since)
        ),
        RecreateHold::Complete
    );
}

#[test]
fn hold_message_lists_uids_nodes_and_reasons() {
    // S47 review F3: the message carries the OBSERVED ack time per entry
    // plus the teardown-since/margin context, so field ops can compare
    // both sides' clocks from the message alone.
    let n1 = node_with_runtime(
        "n1",
        json!([{"podUID": "u-1", "at": "2026-01-01T00:09:59Z",
                    "reason": "teardown-complete"}]),
        json!({}),
    );
    let n2 = node_with_runtime("n2", json!([]), json!({}));
    let nodes = vec![n1, n2];
    let msg = hold_message(
        &[
            (
                TeardownWatch {
                    pod_uid: "u-1".into(),
                    node: "n1".into(),
                },
                "release-unacked",
            ),
            (
                TeardownWatch {
                    pod_uid: "u-2".into(),
                    node: "n2".into(),
                },
                "ledger-referenced",
            ),
        ],
        &nodes,
        Some("2026-01-01T00:10:00Z"),
    );
    assert!(msg.contains("u-1@n1 (release-unacked"), "{msg}");
    assert!(msg.contains("u-2@n2 (ledger-referenced"), "{msg}");
    assert!(msg.contains("ack-at=2026-01-01T00:09:59Z"), "{msg}");
    assert!(msg.contains("ack-at=unset"), "{msg}");
    assert!(msg.contains("teardown-since=2026-01-01T00:10:00Z"), "{msg}");
    assert!(msg.contains("margin=60s"), "{msg}");
    // Unset since (pre-Begin context) renders the explicit "unset".
    let no_since = hold_message(&[], &nodes, None);
    assert!(no_since.contains("teardown-since=unset"), "{no_since}");
}
