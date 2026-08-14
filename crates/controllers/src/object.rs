//! JSON-layer object semantics (T3.1a): identity accessors
//! (name/namespace/resourceVersion), `LabelSelector` matching (both the
//! `matchLabels`/`matchExpressions` and plain-map shapes), ownership
//! (`ownerReferences`), and volatile-metadata-stripping equality for
//! write-if-changed decisions. Hashing/time/randomness live in [`crate::id`]
//! and [`crate::time`].

use serde_json::Value;

/// `"namespace/name"` (or plain `name` for cluster-scoped objects).
pub fn object_key(v: &Value) -> String {
    let name = v
        .pointer("/metadata/name")
        .and_then(Value::as_str)
        .unwrap_or("");
    match namespace(v) {
        Some(ns) if !ns.is_empty() => format!("{ns}/{name}"),
        _ => name.to_string(),
    }
}

/// `metadata.name` if present.
pub fn name(v: &Value) -> Option<&str> {
    v.pointer("/metadata/name").and_then(Value::as_str)
}

/// `metadata.namespace` if present.
pub fn namespace(v: &Value) -> Option<&str> {
    v.pointer("/metadata/namespace").and_then(Value::as_str)
}

/// `metadata.resourceVersion` parsed as the storage mod_revision.
pub fn resource_version(v: &Value) -> Option<u64> {
    v.pointer("/metadata/resourceVersion")
        .and_then(Value::as_str)
        .and_then(|s| s.parse().ok())
}

/// k8s `LabelSelector` match. An **empty** selector (`{}` or only empty
/// `matchLabels`/`matchExpressions`) selects NOTHING: selector-less Services
/// get no managed Endpoints, and Deployment/RS specs require a non-empty
/// selector anyway.
pub fn selector_matches(obj: &Value, selector: &Value) -> bool {
    let Some(sel) = selector.as_object() else {
        return false;
    };
    let labels = obj.pointer("/metadata/labels");
    let mut constraints = 0usize;
    // LabelSelector shape (`matchLabels`/`matchExpressions`): Deployment/RS
    // spec.selector. Plain map shape (Service spec.selector, `app=web`):
    // every key is an equality constraint. Either way zero constraints
    // selects NOTHING (upstream parity).
    let is_selector_shape = sel.contains_key("matchLabels") || sel.contains_key("matchExpressions");
    if !is_selector_shape {
        for (k, want) in sel {
            constraints += 1;
            let got = labels.and_then(|l| l.get(k)).and_then(Value::as_str);
            if got != want.as_str() {
                return false;
            }
        }
        return constraints > 0;
    }
    if let Some(ml) = sel.get("matchLabels").and_then(Value::as_object) {
        for (k, want) in ml {
            constraints += 1;
            let got = labels.and_then(|l| l.get(k)).and_then(Value::as_str);
            if got != want.as_str() {
                return false;
            }
        }
    }
    if let Some(exprs) = sel.get("matchExpressions").and_then(Value::as_array) {
        for expr in exprs {
            constraints += 1;
            if !expression_matches(labels, expr) {
                return false;
            }
        }
    }
    constraints > 0
}

fn expression_matches(labels: Option<&Value>, expr: &Value) -> bool {
    let (Some(key), Some(op)) = (
        expr.get("key").and_then(Value::as_str),
        expr.get("operator").and_then(Value::as_str),
    ) else {
        return false;
    };
    let value = labels.and_then(|l| l.get(key)).and_then(Value::as_str);
    let listed = |v: &str| {
        expr.get("values")
            .and_then(Value::as_array)
            .map(|vs| vs.iter().any(|x| x.as_str() == Some(v)))
    };
    match op {
        "Exists" => value.is_some(),
        "DoesNotExist" => value.is_none(),
        "In" => value.map(|v| listed(v).unwrap_or(false)).unwrap_or(false),
        "NotIn" => value.map(|v| !listed(v).unwrap_or(false)).unwrap_or(true),
        _ => false, // unknown operator: no match
    }
}

/// The `controller: true` ownerReference, if any (clone).
pub fn controller_of(obj: &Value) -> Option<Value> {
    obj.pointer("/metadata/ownerReferences")?
        .as_array()?
        .iter()
        .find(|r| r.get("controller").and_then(Value::as_bool) == Some(true))
        .cloned()
}

/// Build a `controller: true` + `blockOwnerDeletion: true` ownerReference.
pub fn owner_reference(name: &str, uid: &str, kind: &str, api_version: &str) -> Value {
    serde_json::json!({
        "apiVersion": api_version,
        "kind": kind,
        "name": name,
        "uid": uid,
        "controller": true,
        "blockOwnerDeletion": true,
    })
}

/// Pod readiness (moved here from the Endpoints controller for T3.1b: the
/// ReplicaSet status and Deployment availability now share it). Pods with
/// NO conditions at all are ready-by-default -- without kubelet (T4.2)
/// nothing would ever report Ready and every count would stay at zero. A
/// conditions array with an explicit `Ready` condition honors it.
pub fn pod_is_ready(pod: &Value) -> bool {
    match pod.pointer("/status/conditions").and_then(Value::as_array) {
        None => true,
        Some(conditions) => conditions
            .iter()
            .find(|c| c.get("type").and_then(Value::as_str) == Some("Ready"))
            .map(|c| c.get("status").and_then(Value::as_str) == Some("True"))
            .unwrap_or(true),
    }
}

/// Deep equality ignoring volatile metadata: `uid`, `resourceVersion`,
/// `creationTimestamp`, `managedFields`. `status` IS compared (controllers
/// use this for write-if-changed decisions).
pub fn semantic_eq(a: &Value, b: &Value) -> bool {
    strip_volatile(a) == strip_volatile(b)
}

const VOLATILE_META: [&str; 4] = [
    "uid",
    "resourceVersion",
    "creationTimestamp",
    "managedFields",
];

fn strip_volatile(v: &Value) -> Value {
    match v {
        Value::Object(m) => {
            let mut out = serde_json::Map::new();
            for (k, val) in m {
                if k == "metadata" {
                    if let Some(meta) = val.as_object() {
                        let mut mm = meta.clone();
                        for f in VOLATILE_META {
                            mm.remove(f);
                        }
                        out.insert(k.clone(), Value::Object(mm));
                        continue;
                    }
                }
                out.insert(k.clone(), strip_volatile(val));
            }
            Value::Object(out)
        }
        Value::Array(a) => Value::Array(a.iter().map(strip_volatile).collect()),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn pod_with_labels(labels: Value) -> Value {
        json!({"metadata": {"name": "p", "labels": labels}})
    }

    #[test]
    fn selector_match_labels_positive_and_negative() {
        let sel = json!({"matchLabels": {"app": "web"}});
        assert!(selector_matches(
            &pod_with_labels(json!({"app": "web"})),
            &sel
        ));
        assert!(!selector_matches(
            &pod_with_labels(json!({"app": "db"})),
            &sel
        ));
        assert!(!selector_matches(&pod_with_labels(json!({})), &sel));
    }

    #[test]
    fn selector_match_expressions() {
        let pod = pod_with_labels(json!({"tier": "frontend", "env": "prod"}));
        assert!(selector_matches(
            &pod,
            &json!({"matchExpressions": [
                {"key": "tier", "operator": "In", "values": ["frontend", "backend"]},
                {"key": "env", "operator": "NotIn", "values": ["dev"]},
                {"key": "tier", "operator": "Exists"},
                {"key": "zone", "operator": "DoesNotExist"},
            ]})
        ));
        assert!(!selector_matches(
            &pod,
            &json!({"matchExpressions": [
                {"key": "tier", "operator": "In", "values": ["backend"]},
            ]})
        ));
        assert!(!selector_matches(
            &pod,
            &json!({"matchExpressions": [
                {"key": "tier", "operator": "Bogus", "values": []},
            ]})
        ));
    }

    #[test]
    fn empty_selector_selects_nothing() {
        let pod = pod_with_labels(json!({"app": "web"}));
        assert!(!selector_matches(&pod, &json!({})));
        assert!(!selector_matches(&pod, &json!({"matchLabels": {}})));
        assert!(!selector_matches(&pod, &Value::Null));
    }

    #[test]
    fn controller_of_picks_only_controller_true() {
        let obj = json!({"metadata": {"ownerReferences": [
            {"kind": "Deployment", "name": "d", "uid": "1", "controller": false},
            {"kind": "ReplicaSet", "name": "r", "uid": "2", "controller": true},
        ]}});
        let owner = controller_of(&obj).unwrap();
        assert_eq!(
            (owner["kind"].as_str(), owner["name"].as_str()),
            (Some("ReplicaSet"), Some("r"))
        );
        assert!(controller_of(&json!({"metadata": {}})).is_none());
    }

    #[test]
    fn semantic_eq_ignores_volatile_meta_but_not_spec() {
        let a = json!({"metadata": {"uid": "u1", "resourceVersion": "7"}, "spec": {"x": 1},
                       "status": {"replicas": 2}});
        let b = json!({"metadata": {"uid": "u2", "resourceVersion": "9"}, "spec": {"x": 1},
                       "status": {"replicas": 2}});
        assert!(semantic_eq(&a, &b));
        let c = json!({"metadata": {}, "spec": {"x": 2}, "status": {"replicas": 2}});
        assert!(!semantic_eq(&a, &c));
        let d = json!({"metadata": {}, "spec": {"x": 1}, "status": {"replicas": 3}});
        assert!(!semantic_eq(&a, &d), "status must be compared");
    }

    #[test]
    fn pod_is_ready_defaults_to_ready_without_conditions() {
        // No conditions at all -> ready (v1: no kubelet to report Ready).
        assert!(pod_is_ready(&json!({"metadata": {"name": "p"}})));
        // Explicit Ready condition honored both ways.
        assert!(pod_is_ready(&json!({"status": {"conditions": [
            {"type": "PodScheduled", "status": "True"},
            {"type": "Ready", "status": "True"},
        ]}})));
        assert!(!pod_is_ready(&json!({"status": {"conditions": [
            {"type": "Ready", "status": "False"},
        ]}})));
        // Conditions present but no Ready entry -> ready-by-default.
        assert!(pod_is_ready(&json!({"status": {"conditions": [
            {"type": "PodScheduled", "status": "True"},
        ]}})));
    }
}
