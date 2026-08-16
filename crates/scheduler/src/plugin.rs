//! Plugin model (T3.2): the scheduling-cycle data snapshot + the two plugin
//! traits every default policy implements as a **pure function**
//! `(pod, node, snapshot) -> verdict | score` (repo rule: no OOP state —
//! plugins carry no mutable state; scoring weights are config, not state).

use std::sync::Arc;

use serde_json::Value;

/// One node plus the pods already assigned to it (`spec.nodeName` match).
#[derive(Debug, Clone)]
pub struct NodeInfo {
    pub node: Arc<Value>,
    pub pods: Vec<Arc<Value>>,
}

/// Immutable per-cycle world view: every schedulable node + every live pod.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub nodes: Vec<NodeInfo>,
    /// All non-terminal pods (bound or pending) — anti-affinity input.
    pub pods: Vec<Arc<Value>>,
    /// PersistentVolumeClaims (VolumeBinding input; v1 passthrough, Q22).
    pub pvcs: Vec<Arc<Value>>,
}

impl Snapshot {
    /// Assemble a snapshot from informer caches: nodes each with their bound
    /// pods, plus the full pod list (terminal pods excluded).
    pub fn build(pods: &[Arc<Value>], nodes: &[Arc<Value>], pvcs: &[Arc<Value>]) -> Self {
        let live: Vec<Arc<Value>> = pods.iter().filter(|p| !is_terminal(p)).cloned().collect();
        let nodes = nodes
            .iter()
            .map(|n| NodeInfo {
                node: n.clone(),
                pods: live
                    .iter()
                    .filter(|p| pod_on_node(p, &node_name(n).unwrap_or_default()))
                    .cloned()
                    .collect(),
            })
            .collect();
        Snapshot {
            nodes,
            pods: live,
            pvcs: pvcs.to_vec(),
        }
    }

    pub fn node(&self, name: &str) -> Option<&NodeInfo> {
        self.nodes
            .iter()
            .find(|i| node_name(&i.node).map(|n| n == name).unwrap_or(false))
    }

    pub fn node_names(&self) -> Vec<String> {
        self.nodes
            .iter()
            .filter_map(|i| node_name(&i.node))
            .collect()
    }
}

/// Filter verdict: pass, or reject naming the plugin + machine reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Pass,
    Reject {
        plugin: &'static str,
        reason: String,
    },
}

impl Verdict {
    pub fn reject(plugin: &'static str, reason: impl Into<String>) -> Self {
        Verdict::Reject {
            plugin,
            reason: reason.into(),
        }
    }
}

/// A filter plugin (upstream `Filter` extension point). Pure: same inputs,
/// same verdict, no interior mutability.
pub trait Filter: Send + Sync {
    fn name(&self) -> &'static str;
    fn filter(&self, pod: &Value, info: &NodeInfo, snap: &Snapshot) -> Verdict;
}

/// A score plugin (upstream `Score` extension point): `0..=100`, multiplied
/// by `weight()` and summed across nodes for ranking (higher = better).
pub trait Score: Send + Sync {
    fn name(&self) -> &'static str;
    fn weight(&self) -> i64 {
        1
    }
    fn score(&self, pod: &Value, info: &NodeInfo, snap: &Snapshot) -> i64;
}

/// Read `metadata.name` off any object.
pub fn node_name(node: &Value) -> Option<String> {
    node.pointer("/metadata/name")
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

/// Is a pod bound to this node?
pub fn pod_on_node(pod: &Value, node: &str) -> bool {
    pod.pointer("/spec/nodeName")
        .and_then(|v| v.as_str())
        .map(|n| n == node)
        .unwrap_or(false)
}

/// Terminal pods never re-enter scheduling (Succeeded/Failed).
pub fn is_terminal(pod: &Value) -> bool {
    matches!(
        pod.pointer("/status/phase").and_then(|v| v.as_str()),
        Some("Succeeded") | Some("Failed")
    )
}

/// Pending = ours to schedule: no `spec.nodeName`, not terminating, not
/// terminal, and `spec.schedulerName` is ours (empty or `default-scheduler`).
pub fn is_pending(pod: &Value, scheduler_name: &str) -> bool {
    if is_terminal(pod) {
        return false;
    }
    if pod
        .pointer("/spec/nodeName")
        .and_then(|v| v.as_str())
        .map(|n| !n.is_empty())
        .unwrap_or(false)
    {
        return false;
    }
    if pod
        .pointer("/metadata/deletionTimestamp")
        .map(|v| !v.is_null())
        .unwrap_or(false)
    {
        return false;
    }
    let ours = pod
        .pointer("/spec/schedulerName")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    ours.is_empty() || ours == scheduler_name
}

/// The default filter registry (order = upstream framework order).
pub fn default_filters() -> Vec<Box<dyn Filter>> {
    vec![
        Box::new(crate::filters::NodeNameFilter),
        Box::new(crate::filters::NodeUnschedulableFilter),
        Box::new(crate::filters::TaintTolerationFilter),
        Box::new(crate::filters::NodeAffinityFilter),
        Box::new(crate::filters::PodAntiAffinityFilter),
        Box::new(crate::filters::ResourceFitFilter),
        Box::new(crate::filters::VolumeBindingFilter),
    ]
}

/// The default score registry.
pub fn default_scores() -> Vec<Box<dyn Score>> {
    vec![
        Box::new(crate::scores::LeastRequestedScore),
        Box::new(crate::scores::NodeAffinityPreferredScore),
        Box::new(crate::scores::PodAntiAffinityPreferredScore),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn pod(node: Option<&str>) -> Value {
        json!({
            "metadata": {"name": "p", "namespace": "default"},
            "spec": match node { Some(n) => json!({"nodeName": n}), None => json!({}) }
        })
    }

    #[test]
    fn is_pending_requires_no_nodename_and_our_scheduler() {
        assert!(is_pending(&pod(None), "default-scheduler"));
        assert!(!is_pending(&pod(Some("a")), "default-scheduler"));
        let mut named = pod(None);
        named["spec"]["schedulerName"] = json!("custom");
        assert!(!is_pending(&named, "default-scheduler"));
        let mut empty_ok = pod(None);
        empty_ok["spec"]["schedulerName"] = json!("default-scheduler");
        assert!(is_pending(&empty_ok, "default-scheduler"));
    }

    #[test]
    fn is_pending_skips_terminal_and_terminating() {
        let mut dead = pod(None);
        dead["status"]["phase"] = json!("Succeeded");
        assert!(!is_pending(&dead, "default-scheduler"));
        let mut gone = pod(None);
        gone["metadata"]["deletionTimestamp"] = json!("2026-01-01T00:00:00Z");
        assert!(!is_pending(&gone, "default-scheduler"));
    }

    #[test]
    fn snapshot_partitions_pods_by_node() {
        let nodes = vec![
            Arc::new(json!({"metadata": {"name": "a"}})),
            Arc::new(json!({"metadata": {"name": "b"}})),
        ];
        let pods = vec![
            Arc::new(pod(Some("a"))),
            Arc::new(pod(Some("a"))),
            Arc::new(pod(Some("b"))),
            Arc::new(pod(None)),
        ];
        let snap = Snapshot::build(&pods, &nodes, &[]);
        assert_eq!(snap.node("a").unwrap().pods.len(), 2);
        assert_eq!(snap.node("b").unwrap().pods.len(), 1);
        assert_eq!(snap.pods.len(), 4);
        assert_eq!(snap.node_names(), vec!["a", "b"]);
    }
}
