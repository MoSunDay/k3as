//! DaemonSet controller (TODO **T3.1b**).
//!
//! The Node list is the source of truth: one owned pod per **matching**
//! node, pinned via `spec.nodeName`. Placement predicate (v1 subset):
//! `spec.template.spec.nodeSelector` (a plain map: every entry present in
//! the node's labels -- an absent/empty map matches every node) plus the
//! FIRST nodeAffinity `nodeSelectorTerms[0].matchExpressions` term
//! (In/NotIn/Exists/DoesNotExist via [`crate::object::selector_matches`]
//! over a synthesized `{"matchExpressions": [...]}` selector);
//! terminating nodes never match. Reactions: pods on deleted/non-matching
//! nodes are deleted (this IS the node-lifecycle reaction), same-node
//! duplicates keep the lowest name deterministically, and missing
//! placements are created as `<ds>-<rand5>` pods stamped with
//! `pod-template-hash`. Rolling update is the v1 simplification shared
//! with StatefulSet: stale-hash pods are deleted and recreated (no
//! maxSurge/maxUnavailable sequencing, no DaemonSetConditions; the Q-code
//! lands with T3.1b sign-off). Status is write-if-changed via
//! `semantic_eq` (the CAS pattern shared with Deployment/StatefulSet);
//! `numberMisscheduled` is a hard 0 -- we never place on non-matching
//! nodes. JSON-only wire (Q10); in-process storage (Q19).

use std::collections::BTreeSet;
use std::sync::Arc;

use serde_json::{json, Value};
use storage::{Key, KeyPrefix};

use crate::client::Client;
use crate::controllers::{is_terminating, owned_by};
use crate::error::ControllerError;
use crate::id::{add_label, rand_suffix, template_hash};
use crate::object::{
    name, namespace, owner_reference, pod_is_ready, resource_version, selector_matches, semantic_eq,
};

/// Revision label stamped on DaemonSet pods (upstream name).
const TEMPLATE_HASH_LABEL: &str = "pod-template-hash";

/// Reconcile one DaemonSet: placement decision -> deletes -> creates ->
/// status (write-if-changed).
pub async fn reconcile(client: &Arc<dyn Client>, ds: &Value) -> Result<(), ControllerError> {
    let ns = namespace(ds).unwrap_or("default");
    let Some(ds_name) = name(ds) else {
        return Ok(()); // unparseable object: nothing to converge
    };
    if is_terminating(ds) {
        return Ok(()); // deletionTimestamp set: GC territory (T3.1b)
    }
    let template = ds.pointer("/spec/template").cloned().unwrap_or(Value::Null);
    if !template.is_object() {
        return Ok(()); // no pod template: inert (upstream validation error)
    }
    let uid = ds
        .pointer("/metadata/uid")
        .and_then(Value::as_str)
        .unwrap_or("");

    let hash = template_hash(&template);
    let short = &hash[..10];

    let nodes = client.list(&KeyPrefix::new("", "nodes", None)).await?;
    let matching_nodes: BTreeSet<String> = nodes
        .iter()
        .filter(|n| node_matches(n, &template))
        .filter_map(|n| name(n).map(|s| s.to_string()))
        .collect();

    let prefix = KeyPrefix::new("", "pods", Some(ns.to_string()));
    let owned: Vec<Value> = client
        .list(&prefix)
        .await?
        .into_iter()
        .filter(|p| !is_terminating(p) && owned_by(p, "DaemonSet", ds_name, uid))
        .collect();
    let (victims, claimed) = placement_victims(&owned, &matching_nodes, short);
    for victim in victims {
        client
            .delete(&Key::new("", "pods", ns, victim.as_str()))
            .await?;
        tracing::debug!(ds = ds_name, pod = %victim, "deleted daemonset pod");
    }

    for node_name in matching_nodes.difference(&claimed) {
        let pod = pod_body(&template, ns, ds_name, uid, node_name, short);
        let pod_name = name(&pod).unwrap_or_default().to_string();
        match client
            .create(&Key::new("", "pods", ns, pod_name.as_str()), pod)
            .await
        {
            Ok(_) => tracing::debug!(ds = ds_name, node = %node_name, "created daemonset pod"),
            // Suffix collision: the natural requeue creates the rest.
            Err(e) if e.is_already_exists() => {}
            Err(e) => return Err(e),
        }
    }

    let owned: Vec<Value> = client
        .list(&prefix)
        .await?
        .into_iter()
        .filter(|p| !is_terminating(p) && owned_by(p, "DaemonSet", ds_name, uid))
        .collect();
    let placed: Vec<&Value> = owned
        .iter()
        .filter(|p| on_matching_node(p, &matching_nodes))
        .collect();
    let desired = matching_nodes.len() as u64;
    let current = placed.len() as u64;
    let ready = placed.iter().filter(|p| pod_is_ready(p)).count() as u64;
    let updated = placed
        .iter()
        .filter(|p| template_hash_label_of(p) == Some(short))
        .count() as u64;
    let observed = ds
        .pointer("/metadata/generation")
        .and_then(Value::as_u64)
        .unwrap_or(1);
    let new_status = json!({
        "currentNumberScheduled": current,
        "numberMisscheduled": 0, // v1: we never place on non-matching nodes
        "desiredNumberScheduled": desired,
        "numberReady": ready,
        "numberAvailable": ready,
        "updatedNumberScheduled": updated,
        "observedGeneration": observed,
    });
    let current_status = ds.get("status").cloned().unwrap_or(json!({}));
    if !semantic_eq(&current_status, &new_status) {
        let mut next = ds.clone();
        next["status"] = new_status;
        let key = Key::new("apps", "daemonsets", ns, ds_name);
        // Conflict propagates: the worker retries against the fresh cache.
        client.update(&key, next, resource_version(ds)).await?;
        tracing::debug!(ds = ds_name, desired, "daemonset status updated");
    }
    Ok(())
}

/// Placement predicate (v1 subset, see the module doc): not terminating +
/// `nodeSelector` subset of the node's labels + first nodeAffinity
/// `matchExpressions` term. An absent/empty constraint matches EVERY node
/// (contrast [`crate::object::selector_matches`], where an empty selector
/// selects nothing).
fn node_matches(node: &Value, template: &Value) -> bool {
    if is_terminating(node) {
        return false; // deleting node: never a placement target
    }
    if let Some(sel) = template
        .pointer("/spec/nodeSelector")
        .filter(|v| v.as_object().map(|m| !m.is_empty()).unwrap_or(false))
    {
        if !selector_matches(node, sel) {
            return false;
        }
    }
    if let Some(exprs) = template
        .pointer(concat!(
            "/spec/affinity/nodeAffinity/",
            "requiredDuringSchedulingIgnoredDuringExecution/",
            "nodeSelectorTerms/0/matchExpressions",
        ))
        .and_then(Value::as_array)
        .filter(|a| !a.is_empty())
    {
        // selector_matches evaluates matchExpressions over metadata.labels.
        let synth = json!({"matchExpressions": exprs});
        if !selector_matches(node, &synth) {
            return false;
        }
    }
    true
}

/// Pure placement decision (unit-tested): owned pods are examined in name
/// order so the lowest-named pod claims a node; a pod is a VICTIM when its
/// `spec.nodeName` is missing/non-matching, its `pod-template-hash` label
/// is not `short` (rolling update), or its node was already claimed.
fn placement_victims(
    owned: &[Value],
    matching_nodes: &BTreeSet<String>,
    short: &str,
) -> (Vec<String>, BTreeSet<String>) {
    let mut sorted: Vec<&Value> = owned.iter().collect();
    sorted.sort_by_key(|p| name(p).unwrap_or_default().to_string());
    let mut claimed: BTreeSet<String> = BTreeSet::new();
    let mut victims = Vec::new();
    for pod in sorted {
        let Some(pod_name) = name(pod) else { continue };
        let node = pod
            .pointer("/spec/nodeName")
            .and_then(Value::as_str)
            .unwrap_or("");
        let keep = matching_nodes.contains(node)
            && template_hash_label_of(pod) == Some(short)
            && claimed.insert(node.to_string());
        if !keep {
            victims.push(pod_name.to_string());
        }
    }
    (victims, claimed)
}

/// True when the pod is pinned (`spec.nodeName`) to a matching node.
fn on_matching_node(pod: &Value, matching_nodes: &BTreeSet<String>) -> bool {
    pod.pointer("/spec/nodeName")
        .and_then(Value::as_str)
        .map(|n| matching_nodes.contains(n))
        .unwrap_or(false)
}

/// `metadata.labels[pod-template-hash]`, if present.
fn template_hash_label_of(pod: &Value) -> Option<&str> {
    pod.pointer(&format!("/metadata/labels/{TEMPLATE_HASH_LABEL}"))
        .and_then(Value::as_str)
}

/// The pod object for one node: `{ds}-{rand5}` name, template labels +
/// `pod-template-hash`, the template spec with `nodeName` forced to the
/// target node, and the DaemonSet controller ownerReference.
fn pod_body(
    template: &Value,
    ns: &str,
    ds_name: &str,
    uid: &str,
    node: &str,
    short: &str,
) -> Value {
    let pod_name = format!("{ds_name}-{}", rand_suffix(5));
    let mut meta = json!({
        "name": pod_name,
        "namespace": ns,
        "ownerReferences": [owner_reference(ds_name, uid, "DaemonSet", "apps/v1")],
    });
    if let Some(labels) = template
        .pointer("/metadata/labels")
        .filter(|v| v.is_object())
    {
        meta["labels"] = labels.clone();
    }
    let mut spec = template
        .pointer("/spec")
        .filter(|v| v.is_object())
        .cloned()
        .unwrap_or(json!({}));
    spec["nodeName"] = json!(node);
    let mut pod = json!({"apiVersion": "v1", "kind": "Pod", "metadata": meta, "spec": spec});
    add_label(&mut pod, TEMPLATE_HASH_LABEL, short);
    pod
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn make_node(labels: Value) -> Value {
        json!({"apiVersion": "v1", "kind": "Node",
               "metadata": {"name": "n1", "labels": labels}})
    }

    fn template_with(selector: Value) -> Value {
        json!({"metadata": {"labels": {"app": "d"}}, "spec": {"nodeSelector": selector}})
    }

    /// Template whose FIRST nodeAffinity term carries `exprs`.
    fn affinity_template(exprs: Value) -> Value {
        json!({"spec": {"affinity": {"nodeAffinity": {
            "requiredDuringSchedulingIgnoredDuringExecution": {
                "nodeSelectorTerms": [{"matchExpressions": exprs}],
            },
        }}}})
    }

    #[test]
    fn node_selector_subset_matching() {
        let node = make_node(json!({"agent": "init-pro", "zone": "a"}));
        let cases = [
            (json!({"agent": "init-pro"}), true), // subset matches
            (json!({"agent": "init-pro", "zone": "a"}), true),
            (json!({"agent": "other"}), false), // wrong value
            (json!({"agent": "init-pro", "gpu": "yes"}), false), // absent key
            (json!({}), true),                  // empty: every node
        ];
        for (sel, want) in cases {
            assert_eq!(node_matches(&node, &template_with(sel)), want);
        }
        // Absent nodeSelector: matches every node (contrast the
        // empty-LabelSelector-selects-nothing rule for Services).
        assert!(node_matches(&node, &json!({"spec": {}})));
    }

    #[test]
    fn node_affinity_match_expressions_subset() {
        let node = make_node(json!({"zone": "a", "gpu": "true"}));
        let expr = |key: &str, op: &str, values: &[&str]| json!([{"key": key, "operator": op, "values": values}]);
        let cases = [
            (expr("zone", "In", &["a", "b"]), true),
            (expr("zone", "In", &["b"]), false),
            (expr("zone", "NotIn", &["b"]), true),
            (expr("zone", "NotIn", &["a"]), false),
            (expr("gpu", "Exists", &[]), true),
            (expr("tpu", "Exists", &[]), false),
            (expr("tpu", "DoesNotExist", &[]), true),
            (expr("gpu", "DoesNotExist", &[]), false),
        ];
        for (exprs, want) in cases {
            assert_eq!(node_matches(&node, &affinity_template(exprs)), want);
        }
    }

    #[test]
    fn terminating_nodes_never_match_and_only_first_term_honored() {
        let mut n = make_node(json!({"agent": "init-pro"}));
        n["metadata"]["deletionTimestamp"] = json!("2026-08-14T00:00:00Z");
        assert!(!node_matches(
            &n,
            &template_with(json!({"agent": "init-pro"}))
        ));
        // Only the FIRST nodeSelectorTerm is honored (v1 subset): a second,
        // satisfiable term does not rescue the node.
        let both = json!({"spec": {"affinity": {"nodeAffinity": {
            "requiredDuringSchedulingIgnoredDuringExecution": {"nodeSelectorTerms": [
                {"matchExpressions": [{"key": "zone", "operator": "In", "values": ["x"]}]},
                {"matchExpressions": [{"key": "zone", "operator": "In", "values": ["a"]}]},
            ]},
        }}}});
        assert!(!node_matches(&make_node(json!({"zone": "a"})), &both));
    }

    fn owned_pod(pod_name: &str, node: Option<&str>, hash: Option<&str>) -> Value {
        let mut pod = json!({
            "metadata": {"name": pod_name, "ownerReferences": [
                {"kind": "DaemonSet", "name": "ds", "uid": "", "controller": true}]},
            "spec": {},
        });
        if let Some(n) = node {
            pod["spec"]["nodeName"] = json!(n);
        }
        if let Some(h) = hash {
            pod["metadata"]["labels"] = json!({TEMPLATE_HASH_LABEL: h});
        }
        pod
    }

    fn matching(nodes: &[&str]) -> BTreeSet<String> {
        nodes.iter().map(|n| n.to_string()).collect()
    }

    #[test]
    fn placement_keeps_one_pod_per_node_lowest_name_wins() {
        let owned = vec![
            owned_pod("ds-zzzzz", Some("n1"), Some("aaaaa1111")),
            owned_pod("ds-aaaaa", Some("n1"), Some("aaaaa1111")),
        ];
        let (victims, claimed) = placement_victims(&owned, &matching(&["n1", "n2"]), "aaaaa1111");
        assert_eq!(victims, vec!["ds-zzzzz"]); // highest name is the victim
        assert_eq!(claimed, matching(&["n1"]));
    }

    #[test]
    fn placement_deletes_off_node_and_missing_node_name_pods() {
        let owned = vec![
            owned_pod("ds-a", Some("n1"), Some("aaaaa1111")), // keep
            owned_pod("ds-b", Some("gone"), Some("aaaaa1111")), // node deleted
            owned_pod("ds-c", Some("n9"), Some("aaaaa1111")), // never matched
            owned_pod("ds-d", None, Some("aaaaa1111")),       // unschedulable
        ];
        let (victims, claimed) = placement_victims(&owned, &matching(&["n1"]), "aaaaa1111");
        assert_eq!(victims, vec!["ds-b", "ds-c", "ds-d"]);
        assert_eq!(claimed, matching(&["n1"]));
    }

    #[test]
    fn placement_rolling_update_deletes_stale_template_hash() {
        let owned = vec![
            owned_pod("ds-a", Some("n1"), Some("oldhash0000")),
            owned_pod("ds-b", Some("n2"), None), // no label: also stale
        ];
        let (victims, claimed) = placement_victims(&owned, &matching(&["n1", "n2"]), "newhash1111");
        assert_eq!(victims, vec!["ds-a", "ds-b"]);
        assert!(claimed.is_empty()); // the create path recreates both
    }

    #[test]
    fn pod_body_pins_node_and_stamps_template_hash() {
        let template = json!({
            "metadata": {"labels": {"app": "d"}},
            "spec": {"containers": [{"name": "c", "image": "nginx"}]},
        });
        let pod = pod_body(&template, "default", "ds", "u1", "n1", "abcd1234ef");
        assert!(name(&pod).unwrap_or("").starts_with("ds-"));
        assert_eq!(pod["metadata"]["namespace"], "default");
        assert_eq!(pod["metadata"]["labels"]["app"], "d");
        assert_eq!(pod["metadata"]["labels"][TEMPLATE_HASH_LABEL], "abcd1234ef");
        assert_eq!(pod["spec"]["nodeName"], "n1"); // placement forced
        assert_eq!(pod["spec"]["containers"][0]["image"], "nginx");
        assert_eq!(pod["metadata"]["ownerReferences"][0]["kind"], "DaemonSet");
        assert_eq!(pod["metadata"]["ownerReferences"][0]["controller"], true);
    }
}
