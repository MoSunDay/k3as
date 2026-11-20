//! INF4 (T7.6, Q62) reconcile-level tests for the safe Recreate hold:
//! real [`reconcile`] passes over an [`EmbeddedStorage`]-backed client with
//! fake Node objects shaped exactly like the node-status contract
//! (`status.initProRuntime.{gpuLedger,released}`). The matrix:
//! (i) held with no release record, (ii) fresh ack + clean ledger proceeds,
//! (iii) stale ack stays held, (iv) a live gpuLedger allocation (on ANY
//! node) stays held, (v) ordinary pods keep the upstream fast path, and
//! (vi) the Q58-canary-shaped RollingUpdate rollout never engages the hold.

use super::*;
use crate::client::StorageClient;
use serde_json::json;
use std::sync::Arc;
use storage::EmbeddedStorage;

const T0: u64 = 1_700_000_000;

fn dep_key() -> Key {
    Key::new("apps", "deployments", "default", "llama")
}

fn dep_spec(template: Value, strategy: &str) -> Value {
    json!({
        "apiVersion": "apps/v1", "kind": "Deployment",
        "metadata": {"name": "llama", "namespace": "default",
                     "uid": "dep-uid", "generation": 2},
        "spec": {
            "replicas": 1,
            "strategy": {"type": strategy},
            "selector": {"matchLabels": {"app": "llama"}},
            "template": template,
        },
    })
}

/// The T7.6 inference pod shape (Q60 host-direct rootfs + Q61 whole GPU).
fn template_v1() -> Value {
    json!({
        "metadata": {"labels": {"app": "llama"},
                     "annotations": {"init-pro.io/rootfs": "/"}},
        "spec": {"containers": [{
            "name": "server", "image": "ignored",
            "resources": {"requests": {"nvidia.com/gpu": 1}},
        }]},
    })
}

/// Same workload, one bumped env var: a different pod-template hash.
fn template_v2() -> Value {
    let mut v2 = template_v1();
    v2["spec"]["containers"][0]["env"] = json!([{"name": "REV", "value": "2"}]);
    v2
}

fn rs_name_of(template: &Value) -> String {
    format!("llama-{}", &template_hash(template)[..10])
}

async fn setup() -> Arc<dyn Client> {
    let store = Arc::new(EmbeddedStorage::new());
    let client: Arc<dyn Client> = Arc::new(StorageClient::new(store));
    client
        .create(
            &Key::new("", "nodes", "", "n1"),
            json!({"apiVersion": "v1", "kind": "Node",
                   "metadata": {"name": "n1"},
                   "status": {"initProRuntime": {}}}),
        )
        .await
        .unwrap();
    client
}

async fn get(client: &Arc<dyn Client>, key: Key) -> Value {
    client.get(&key).await.unwrap().unwrap_or(json!({}))
}

async fn rs_spec(client: &Arc<dyn Client>, rs: &str) -> u64 {
    get(client, Key::new("apps", "replicasets", "default", rs))
        .await
        .pointer("/spec/replicas")
        .and_then(Value::as_u64)
        .unwrap_or(0)
}

fn annotation<'a>(dep: &'a Value, key: &str) -> Option<&'a str> {
    dep.pointer("/metadata/annotations")
        .and_then(|a| a.get(key))
        .and_then(Value::as_str)
}

/// Simulate the ReplicaSet controller: seed one live pod for `rs` of the
/// given shape and mark the RS status ready.
async fn seed_old_pod(client: &Arc<dyn Client>, rs: &str, uid: &str, host_direct: bool, gpu: bool) {
    let mut pod = json!({
        "apiVersion": "v1", "kind": "Pod",
        "metadata": {"name": "llama-old", "namespace": "default", "uid": uid,
                     "ownerReferences": [
                         {"kind": "ReplicaSet", "name": rs,
                          "uid": "", "controller": true}]},
        "spec": {"nodeName": "n1", "containers": [{
            "name": "server", "image": "ignored",
            "resources": {"requests": {}}}]},
    });
    if host_direct {
        pod["metadata"]["annotations"] = json!({"init-pro.io/rootfs": "/"});
    }
    if gpu {
        pod["spec"]["containers"][0]["resources"]["requests"] = json!({"nvidia.com/gpu": 1});
    }
    client
        .create(&Key::new("", "pods", "default", "llama-old"), pod)
        .await
        .unwrap();
    let mut with_status = get(client, Key::new("apps", "replicasets", "default", rs)).await;
    with_status["status"] = json!({"replicas": 1, "readyReplicas": 1, "availableReplicas": 1});
    client
        .update(
            &Key::new("apps", "replicasets", "default", rs),
            with_status.clone(),
            resource_version(&with_status),
        )
        .await
        .unwrap();
}

/// Replace a node's `status.initProRuntime` wholesale (ledger/release ack).
async fn set_node_runtime(client: &Arc<dyn Client>, node: &str, init_pro: Value) {
    let key = Key::new("", "nodes", "", node);
    let mut n = get(client, key.clone()).await;
    n["status"]["initProRuntime"] = init_pro;
    client
        .update(&key, n.clone(), resource_version(&n))
        .await
        .unwrap();
}

/// Simulate the drain finishing: old RS status zeroes + the pod object is
/// gone (upstream's "runtime is gone" signal — NOT sufficient for INF4).
async fn drain_old(client: &Arc<dyn Client>, old_rs: &str) {
    let mut rs = get(client, Key::new("apps", "replicasets", "default", old_rs)).await;
    rs["status"] = json!({"replicas": 0, "readyReplicas": 0, "availableReplicas": 0});
    client
        .update(
            &Key::new("apps", "replicasets", "default", old_rs),
            rs.clone(),
            resource_version(&rs),
        )
        .await
        .unwrap();
    client
        .delete(&Key::new("", "pods", "default", "llama-old"))
        .await
        .unwrap();
}

/// Boot v1@1 replica, seed one old pod of the given shape, roll the
/// template to v2 and run the pass at T0+10 (the hold's Begin instant).
/// Returns the Deployment as stored after that pass.
async fn begin_hold(
    client: &Arc<dyn Client>,
    strategy: &str,
    host_direct: bool,
    gpu: bool,
) -> Value {
    let dep = client
        .create(&dep_key(), dep_spec(template_v1(), strategy))
        .await
        .unwrap();
    reconcile(client, &dep, T0).await.unwrap(); // creates llama-<h1> at 1
    seed_old_pod(
        client,
        &rs_name_of(&template_v1()),
        "pod-old-uid",
        host_direct,
        gpu,
    )
    .await;
    let mut dep = get(client, dep_key()).await;
    dep["spec"]["template"] = template_v2();
    client
        .update(&dep_key(), dep.clone(), resource_version(&dep))
        .await
        .unwrap();
    let dep = get(client, dep_key()).await;
    reconcile(client, &dep, T0 + 10).await.unwrap();
    get(client, dep_key()).await
}

fn progressing(dep: &Value) -> (String, String) {
    dep.pointer("/status/conditions")
        .and_then(Value::as_array)
        .and_then(|cs| {
            cs.iter()
                .find(|c| c.get("type").and_then(Value::as_str) == Some("Progressing"))
        })
        .map(|c| {
            (
                c.get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                c.get("message")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            )
        })
        .unwrap_or_default()
}

#[tokio::test]
async fn i_recreate_hold_pins_new_rs_until_node_release_ack() {
    let client = setup().await;
    let dep = begin_hold(&client, "Recreate", true, true).await;
    // Begin recorded on the Deployment exactly once, watch set persisted.
    assert_eq!(
        annotation(&dep, TEARDOWN_SINCE_ANNOTATION),
        Some(now_rfc3339(T0 + 10).as_str())
    );
    let pending = annotation(&dep, TEARDOWN_PENDING_ANNOTATION).unwrap_or("");
    assert!(
        pending.contains("pod-old-uid") && pending.contains("n1"),
        "pending={pending}"
    );
    // New RS pinned at 0, old RS draining, hold surfaced on Progressing.
    assert_eq!(rs_spec(&client, &rs_name_of(&template_v2())).await, 0);
    assert_eq!(rs_spec(&client, &rs_name_of(&template_v1())).await, 0);
    let (reason, msg) = progressing(&dep);
    assert_eq!(reason, "OldRuntimeTeardownPending");
    assert!(msg.contains("pod-old-uid@n1"), "msg={msg}");
}

#[tokio::test]
async fn ii_fresh_release_ack_and_clean_ledger_complete_the_hold() {
    let client = setup().await;
    begin_hold(&client, "Recreate", true, true).await;
    // Node acks the teardown AFTER teardown-since (T0+10) and the ledger
    // no longer references the podUID.
    set_node_runtime(
        &client,
        "n1",
        // Fresh ack: beyond since (T0+10) + CONFIRM_MARGIN_SECS (60s,
        // S47 review F3) - T0+20 would now sit inside the margin.
        json!({"released": [{"podUID": "pod-old-uid",
                             "at": now_rfc3339(T0 + 80),
                             "reason": "teardown-complete"}]}),
    )
    .await;
    drain_old(&client, &rs_name_of(&template_v1())).await;
    let dep = get(&client, dep_key()).await;
    reconcile(&client, &dep, T0 + 30).await.unwrap();
    let dep = get(&client, dep_key()).await;
    // Annotations cleared and the new RS scales to desired.
    assert_eq!(annotation(&dep, TEARDOWN_SINCE_ANNOTATION), None);
    assert_eq!(annotation(&dep, TEARDOWN_PENDING_ANNOTATION), None);
    assert_eq!(rs_spec(&client, &rs_name_of(&template_v2())).await, 1);
    assert_ne!(progressing(&dep).0, "OldRuntimeTeardownPending");
}

#[tokio::test]
async fn iii_stale_release_ack_keeps_the_hold() {
    let client = setup().await;
    begin_hold(&client, "Recreate", true, true).await;
    // The release record predates teardown-since: a leftover from an
    // EARLIER teardown, never a confirmation of this one.
    set_node_runtime(
        &client,
        "n1",
        json!({"released": [{"podUID": "pod-old-uid",
                             "at": now_rfc3339(T0 + 5),
                             "reason": "teardown-complete"}]}),
    )
    .await;
    drain_old(&client, &rs_name_of(&template_v1())).await;
    let dep = get(&client, dep_key()).await;
    reconcile(&client, &dep, T0 + 30).await.unwrap();
    let dep = get(&client, dep_key()).await;
    assert!(annotation(&dep, TEARDOWN_SINCE_ANNOTATION).is_some());
    assert_eq!(rs_spec(&client, &rs_name_of(&template_v2())).await, 0);
    let (reason, msg) = progressing(&dep);
    assert_eq!(reason, "OldRuntimeTeardownPending");
    assert!(msg.contains("release-unacked"), "msg={msg}");
}

#[tokio::test]
async fn iv_ledger_allocation_on_any_node_keeps_the_hold() {
    let client = setup().await;
    begin_hold(&client, "Recreate", true, true).await;
    set_node_runtime(
        &client,
        "n1",
        // Ack beyond since + margin so the ONLY blocker is the foreign
        // ledger entry (an in-margin ack would also read release-unacked,
        // S47 review F3, and hide the reason under test here).
        json!({"released": [{"podUID": "pod-old-uid",
                             "at": now_rfc3339(T0 + 80),
                             "reason": "teardown-complete"}]}),
    )
    .await;
    // The hosting node acked, but ANOTHER node's ledger still carries the
    // allocation: teardown is provably not complete.
    client
        .create(
            &Key::new("", "nodes", "", "n2"),
            json!({"apiVersion": "v1", "kind": "Node",
                   "metadata": {"name": "n2"},
                   "status": {"initProRuntime": {"gpuLedger": {
                       "nvidia.com/gpu": {"total": 1, "allocations": {
                           "pod-old-uid": {"server": [0]}}}}}}}),
        )
        .await
        .unwrap();
    drain_old(&client, &rs_name_of(&template_v1())).await;
    let dep = get(&client, dep_key()).await;
    reconcile(&client, &dep, T0 + 30).await.unwrap();
    let dep = get(&client, dep_key()).await;
    assert!(annotation(&dep, TEARDOWN_SINCE_ANNOTATION).is_some());
    assert_eq!(rs_spec(&client, &rs_name_of(&template_v2())).await, 0);
    let (reason, msg) = progressing(&dep);
    assert_eq!(reason, "OldRuntimeTeardownPending");
    assert!(msg.contains("ledger-referenced"), "msg={msg}");
}

#[tokio::test]
async fn v_ordinary_recreate_rolls_out_without_any_hold_state() {
    let client = setup().await;
    // No host-direct annotation, no GPU request: pure upstream fast path.
    let dep = begin_hold(&client, "Recreate", false, false).await;
    assert_eq!(annotation(&dep, TEARDOWN_SINCE_ANNOTATION), None);
    assert_eq!(annotation(&dep, TEARDOWN_PENDING_ANNOTATION), None);
    assert_ne!(progressing(&dep).0, "OldRuntimeTeardownPending");
    // Upstream Recreate gating (old capacity blocks the scale-up) — the
    // ordinary drain then one-pass scale-up completes with no hold reads.
    assert_eq!(rs_spec(&client, &rs_name_of(&template_v2())).await, 0);
    drain_old(&client, &rs_name_of(&template_v1())).await;
    let dep = get(&client, dep_key()).await;
    reconcile(&client, &dep, T0 + 30).await.unwrap();
    let dep = get(&client, dep_key()).await;
    assert_eq!(annotation(&dep, TEARDOWN_SINCE_ANNOTATION), None);
    assert_eq!(rs_spec(&client, &rs_name_of(&template_v2())).await, 1);
    assert_ne!(progressing(&dep).0, "OldRuntimeTeardownPending");
}

#[tokio::test]
async fn vi_rolling_update_canary_shape_never_engages_the_hold() {
    let client = setup().await;
    // Q58 canary clones are RollingUpdate Deployments; even with
    // host-direct/GPU old pods the hold is Recreate-only.
    let dep = begin_hold(&client, "RollingUpdate", true, true).await;
    assert_eq!(annotation(&dep, TEARDOWN_SINCE_ANNOTATION), None);
    assert_eq!(annotation(&dep, TEARDOWN_PENDING_ANNOTATION), None);
    let (reason, _) = progressing(&dep);
    assert_ne!(reason, "OldRuntimeTeardownPending");
    // RollingUpdate paces instead: the new RS surges to 1 while the old
    // RS keeps its replica (no Recreate gate).
    assert_eq!(rs_spec(&client, &rs_name_of(&template_v2())).await, 1);
    assert_eq!(rs_spec(&client, &rs_name_of(&template_v1())).await, 1);
}
