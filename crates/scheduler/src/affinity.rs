//! Affinity/toleration matching shared by filters + scores (T3.2).
//!
//! All functions are pure over raw JSON (`serde_json::Value`, **Q10**):
//!  - `node_selector_matches` — `spec.nodeSelector` plain map (subset match)
//!  - `node_affinity_required_matches` — required nodeAffinity: terms ORed,
//!    expressions within a term ANDed (the full upstream semantics; DaemonSet
//!    placement in controllers checks only the first term and is unchanged)
//!  - `node_affinity_preferred_weight` — preferred terms: sum matched weights
//!  - `tolerates_taint` / `tolerates_all` — taint/toleration matrix
//!  - pod anti-affinity: required violation check + preferred penalty count

use serde_json::Value;

use crate::plugin::{NodeInfo, Snapshot};

/// `spec.nodeSelector` (plain `{label: value}` map): every entry must be a
/// node label with equal value. An absent/empty selector matches every node.
pub fn node_selector_matches(node: &Value, selector: &Value) -> bool {
    let Some(map) = selector.as_object() else {
        return true;
    };
    let labels = node.pointer("/metadata/labels");
    map.iter().all(|(k, v)| {
        // Label keys may contain '/' (kubernetes.io/hostname) which JSON
        // pointer treats as a separator — look up via the labels object.
        labels
            .and_then(|l| l.get(k))
            .map(|nv| nv == v || (nv.is_string() && v.is_string() && nv.as_str() == v.as_str()))
            .unwrap_or(false)
    })
}

/// LabelSelector matching against a labels object: `matchLabels` (subset) +
/// `matchExpressions` (In/NotIn/Exists/DoesNotExist, ANDed).
pub fn label_selector_matches(labels: &Value, selector: &Value) -> bool {
    if let Some(ml) = selector.get("matchLabels").and_then(|v| v.as_object()) {
        for (k, v) in ml {
            let got = labels.get(k);
            if got
                .map(|g| g != v && g.as_str() != v.as_str())
                .unwrap_or(true)
            {
                return false;
            }
        }
    }
    if let Some(exprs) = selector.get("matchExpressions").and_then(|v| v.as_array()) {
        for e in exprs {
            let key = e.get("key").and_then(|v| v.as_str()).unwrap_or_default();
            let op = e.get("operator").and_then(|v| v.as_str()).unwrap_or("");
            let values: Vec<&str> = e
                .get("values")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|x| x.as_str()).collect())
                .unwrap_or_default();
            let got = labels.get(key).and_then(|v| v.as_str());
            let ok = match op {
                "In" => got.map(|g| values.contains(&g)).unwrap_or(false),
                "NotIn" => got.map(|g| !values.contains(&g)).unwrap_or(true),
                "Exists" => got.is_some(),
                "DoesNotExist" => got.is_none(),
                _ => false,
            };
            if !ok {
                return false;
            }
        }
    }
    true
}

/// `spec.affinity.nodeAffinity.requiredDuringSchedulingIgnoredDuringExecution`:
/// node passes iff it matches **at least one** `nodeSelectorTerms` entry
/// (terms ORed; matchExpressions within a term ANDed). Missing affinity or an
/// empty block matches every node; a present-but-empty `nodeSelectorTerms`
/// list matches none (conservative; upstream validation forbids it anyway).
pub fn node_affinity_required_matches(node: &Value, pod: &Value) -> bool {
    let terms = pod
        .pointer("/spec/affinity/nodeAffinity/requiredDuringSchedulingIgnoredDuringExecution/nodeSelectorTerms")
        .and_then(|v| v.as_array());
    let Some(terms) = terms else { return true };
    if terms.is_empty() {
        return false;
    }
    let labels = node
        .pointer("/metadata/labels")
        .cloned()
        .unwrap_or(Value::Null);
    terms.iter().any(|term| {
        let mut ok = true;
        if let Some(ml) = term.get("matchLabels").and_then(|v| v.as_object()) {
            for (k, v) in ml {
                if labels
                    .get(k)
                    .map(|g| g != v && g.as_str() != v.as_str())
                    .unwrap_or(true)
                {
                    ok = false;
                }
            }
        }
        if let Some(exprs) = term.get("matchExpressions").and_then(|v| v.as_array()) {
            let sel = serde_json::json!({ "matchExpressions": exprs });
            ok = ok && label_selector_matches(&labels, &sel);
        }
        ok
    })
}

/// Sum of weights of **matching** `preferred...` nodeAffinity terms.
pub fn node_affinity_preferred_weight(node: &Value, pod: &Value) -> i64 {
    let Some(preferred) = pod
        .pointer("/spec/affinity/nodeAffinity/preferredDuringSchedulingIgnoredDuringExecution")
        .and_then(|v| v.as_array())
    else {
        return 0;
    };
    let labels = node
        .pointer("/metadata/labels")
        .cloned()
        .unwrap_or(Value::Null);
    preferred
        .iter()
        .filter(|wt| {
            let term = wt.get("preference").cloned().unwrap_or(Value::Null);
            let sel = merge_term_selector(&term);
            label_selector_matches(&labels, &sel)
        })
        .map(|wt| wt.get("weight").and_then(|w| w.as_i64()).unwrap_or(0))
        .sum()
}

/// Does one toleration cover one taint? (operator Exists / Equal on key,
/// optional value; `effect: ""` tolerates every effect.)
pub fn tolerates_taint(toleration: &Value, taint: &Value) -> bool {
    let t_key = taint.get("key").and_then(|v| v.as_str()).unwrap_or("");
    let t_effect = taint.get("effect").and_then(|v| v.as_str()).unwrap_or("");
    let t_value = taint.get("value").and_then(|v| v.as_str()).unwrap_or("");
    let key = toleration.get("key").and_then(|v| v.as_str()).unwrap_or("");
    let op = toleration
        .get("operator")
        .and_then(|v| v.as_str())
        .unwrap_or("Equal");
    let effect = toleration
        .get("effect")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if !effect.is_empty() && effect != t_effect {
        return false;
    }
    // An empty toleration key with operator Exists tolerates everything.
    if key.is_empty() {
        return op == "Exists";
    }
    if key != t_key {
        return false;
    }
    match op {
        "Exists" => true,
        _ => {
            let v = toleration
                .get("value")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            v == t_value
        }
    }
}

/// Are all NoSchedule/NoExecute taints on the node tolerated?
/// (PreferNoSchedule is soft — upstream scores it, v1 documents it out.)
pub fn tolerates_all(pod: &Value, node: &Value) -> bool {
    let Some(taints) = node.pointer("/spec/taints").and_then(|v| v.as_array()) else {
        return true;
    };
    let tolerations: Vec<&Value> = pod
        .pointer("/spec/tolerations")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().collect())
        .unwrap_or_default();
    taints
        .iter()
        .filter(|t| {
            matches!(
                t.get("effect").and_then(|v| v.as_str()),
                Some("NoSchedule") | Some("NoExecute")
            )
        })
        .all(|t| tolerations.iter().any(|tl| tolerates_taint(tl, t)))
}

/// Anti-affinity required violation: for every required term, any live pod
/// whose labels match the term's `labelSelector` **and** that sits in the
/// same topology segment (`topologyKey` label value equal on both nodes)
/// blocks this node. A node missing the topology label cannot satisfy a
/// required term (upstream semantics).
pub fn anti_affinity_violated(pod: &Value, info: &NodeInfo, snap: &Snapshot) -> bool {
    let Some(terms) = pod
        .pointer("/spec/affinity/podAntiAffinity/requiredDuringSchedulingIgnoredDuringExecution")
        .and_then(|v| v.as_array())
    else {
        return false;
    };
    let node_labels = info
        .node
        .pointer("/metadata/labels")
        .cloned()
        .unwrap_or(Value::Null);
    let me = (
        pod.pointer("/metadata/name").and_then(|v| v.as_str()),
        pod.pointer("/metadata/namespace").and_then(|v| v.as_str()),
    );
    for term in terms {
        let Some(topology_key) = term.get("topologyKey").and_then(|v| v.as_str()) else {
            continue;
        };
        let Some(segment) = node_labels.get(topology_key) else {
            return true; // cannot evaluate the term on this node
        };
        let selector = term.get("labelSelector").cloned().unwrap_or(Value::Null);
        for other in &snap.pods {
            let other_id = (
                other.pointer("/metadata/name").and_then(|v| v.as_str()),
                other
                    .pointer("/metadata/namespace")
                    .and_then(|v| v.as_str()),
            );
            if other_id == me {
                continue;
            }
            let other_labels = other
                .pointer("/metadata/labels")
                .cloned()
                .unwrap_or(Value::Null);
            if !label_selector_matches(&other_labels, &selector) {
                continue;
            }
            let Some(other_node_name) = other.pointer("/spec/nodeName").and_then(|v| v.as_str())
            else {
                continue; // pending pod: occupies no segment yet
            };
            let same_segment = snap
                .node(other_node_name)
                .and_then(|i| i.node.pointer("/metadata/labels"))
                .and_then(|l| l.get(topology_key))
                .map(|v| *v == *segment)
                .unwrap_or(false);
            if same_segment {
                return true;
            }
        }
    }
    false
}

/// Count of **soft** anti-affinity conflicts on this node (for scoring).
pub fn anti_affinity_penalty(pod: &Value, info: &NodeInfo) -> i64 {
    let Some(preferred) = pod
        .pointer("/spec/affinity/podAntiAffinity/preferredDuringSchedulingIgnoredDuringExecution")
        .and_then(|v| v.as_array())
    else {
        return 0;
    };
    let me = (
        pod.pointer("/metadata/name"),
        pod.pointer("/metadata/namespace"),
    );
    let node_name = info
        .node
        .pointer("/metadata/name")
        .cloned()
        .unwrap_or(Value::Null);
    let mut conflicts = 0i64;
    for wt in preferred {
        let Some(term) = wt.get("podAffinityTerm") else {
            continue;
        };
        let selector = term.get("labelSelector").cloned().unwrap_or(Value::Null);
        for other in &info.pods {
            if (
                other.pointer("/metadata/name"),
                other.pointer("/metadata/namespace"),
            ) == me
            {
                continue;
            }
            let other_labels = other
                .pointer("/metadata/labels")
                .cloned()
                .unwrap_or(Value::Null);
            if label_selector_matches(&other_labels, &selector)
                && other.pointer("/spec/nodeName") == Some(&node_name)
            {
                conflicts += 1;
            }
        }
    }
    conflicts
}

/// Merge a nodeSelectorTerm (`matchLabels` + `matchExpressions`) into a
/// single LabelSelector shape for [`label_selector_matches`].
fn merge_term_selector(term: &Value) -> Value {
    let mut sel = serde_json::Map::new();
    if let Some(ml) = term.get("matchLabels") {
        sel.insert("matchLabels".into(), ml.clone());
    }
    if let Some(me) = term.get("matchExpressions") {
        sel.insert("matchExpressions".into(), me.clone());
    }
    Value::Object(sel)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn node(labels: Value) -> Value {
        json!({"metadata": {"name": "n", "labels": labels}})
    }

    #[test]
    fn node_selector_requires_subset() {
        let n = node(json!({"disk": "ssd", "zone": "z1"}));
        assert!(node_selector_matches(&n, &json!({"disk": "ssd"})));
        assert!(!node_selector_matches(&n, &json!({"disk": "nvme"})));
        assert!(node_selector_matches(&n, &json!({})));
        assert!(node_selector_matches(&n, &Value::Null));
    }

    #[test]
    fn required_node_affinity_ors_terms_and_ands_expressions() {
        let n = node(json!({"disk": "ssd"}));
        let pod = json!({"spec": {"affinity": {"nodeAffinity": {
            "requiredDuringSchedulingIgnoredDuringExecution": {"nodeSelectorTerms": [
                {"matchExpressions": [{"key": "disk", "operator": "In", "values": ["nvme"]}]},
                {"matchExpressions": [{"key": "disk", "operator": "In", "values": ["ssd"]}]},
            ]}
        }}}});
        assert!(node_affinity_required_matches(&n, &pod));
        let only_bad = json!({"spec": {"affinity": {"nodeAffinity": {
            "requiredDuringSchedulingIgnoredDuringExecution": {"nodeSelectorTerms": [
                {"matchExpressions": [{"key": "disk", "operator": "In", "values": ["nvme"]}]}
            ]}
        }}}});
        assert!(!node_affinity_required_matches(&n, &only_bad));
        let empty_terms = json!({"spec": {"affinity": {"nodeAffinity": {
            "requiredDuringSchedulingIgnoredDuringExecution": {"nodeSelectorTerms": []}
        }}}});
        assert!(!node_affinity_required_matches(&n, &empty_terms));
        assert!(node_affinity_required_matches(&n, &json!({"spec": {}})));
    }

    #[test]
    fn preferred_node_affinity_sums_matched_weights() {
        let n = node(json!({"disk": "ssd"}));
        let pod = json!({"spec": {"affinity": {"nodeAffinity": {
            "preferredDuringSchedulingIgnoredDuringExecution": [
                {"weight": 10, "preference": {"matchExpressions": [{"key": "disk", "operator": "In", "values": ["ssd"]}]}},
                {"weight": 90, "preference": {"matchExpressions": [{"key": "disk", "operator": "In", "values": ["nvme"]}]}},
            ]
        }}}});
        assert_eq!(node_affinity_preferred_weight(&n, &pod), 10);
    }

    #[test]
    fn toleration_matrix() {
        let taint = json!({"key": "node.kubernetes.io/not-ready", "effect": "NoSchedule"});
        assert!(tolerates_taint(
            &json!({"key": "node.kubernetes.io/not-ready", "operator": "Exists"}),
            &taint
        ));
        assert!(tolerates_taint(&json!({"operator": "Exists"}), &taint));
        assert!(!tolerates_taint(
            &json!({"key": "other", "operator": "Exists"}),
            &taint
        ));
        let valued = json!({"key": "gpu", "value": "true", "effect": "NoSchedule"});
        assert!(tolerates_taint(
            &json!({"key": "gpu", "operator": "Equal", "value": "true"}),
            &valued
        ));
        assert!(!tolerates_taint(
            &json!({"key": "gpu", "operator": "Equal", "value": "false"}),
            &valued
        ));
        let pod = json!({"spec": {"tolerations": [{"operator": "Exists"}]}});
        let tainted = json!({"metadata": {"name": "n"}, "spec": {"taints": [
            {"key": "a", "effect": "NoSchedule"},
            {"key": "b", "effect": "PreferNoSchedule"},
        ]}});
        assert!(tolerates_all(&pod, &tainted));
    }

    #[test]
    fn anti_affinity_detects_colocated_conflict() {
        use std::sync::Arc;
        let n =
            Arc::new(json!({"metadata": {"name": "n", "labels": {"kubernetes.io/hostname": "n"}}}));
        let existing = Arc::new(json!({
            "metadata": {"name": "web-1", "namespace": "default", "labels": {"app": "web"}},
            "spec": {"nodeName": "n"}
        }));
        let info = NodeInfo {
            node: n.clone(),
            pods: vec![existing.clone()],
        };
        let snap = Snapshot::build(&[existing], &[n], &[]);
        let conflicting = json!({
            "metadata": {"name": "web-2", "namespace": "default"},
            "spec": {"affinity": {"podAntiAffinity": {
                "requiredDuringSchedulingIgnoredDuringExecution": [{
                    "topologyKey": "kubernetes.io/hostname",
                    "labelSelector": {"matchLabels": {"app": "web"}}
                }]
            }}}
        });
        assert!(anti_affinity_violated(&conflicting, &info, &snap));
        let other_app = json!({
            "metadata": {"name": "db-1", "namespace": "default"},
            "spec": {"affinity": {"podAntiAffinity": {
                "requiredDuringSchedulingIgnoredDuringExecution": [{
                    "topologyKey": "kubernetes.io/hostname",
                    "labelSelector": {"matchLabels": {"app": "db"}}
                }]
            }}}
        });
        assert!(!anti_affinity_violated(&other_app, &info, &snap));
    }
}
