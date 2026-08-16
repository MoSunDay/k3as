//! Default score plugins (T3.2): pure `(pod, node, snapshot) -> 0..=100`,
//! weighted-summed by the cycle. `LeastRequested` spreads load; the two
//! preferred-affinity bonuses steer toward soft constraints.

use serde_json::Value;

use crate::affinity;
use crate::plugin::{NodeInfo, Score, Snapshot};
use crate::resources;

/// `LeastRequestedPriority` (upstream): per resource
/// `(allocatable - requested) / allocatable * 100`, averaged over cpu +
/// memory. A logical node (unbounded, Q23) scores a flat 100 — it can always
/// absorb the pod, so it wins over any partially-used real node. That is
/// the intended v1 default for headless clusters.
pub struct LeastRequestedScore;

impl Score for LeastRequestedScore {
    fn name(&self) -> &'static str {
        "LeastRequested"
    }
    fn weight(&self) -> i64 {
        1
    }
    fn score(&self, _pod: &Value, info: &NodeInfo, _snap: &Snapshot) -> i64 {
        let mut sum = 0.0;
        let mut count = 0.0;
        for resource in ["cpu", "memory"] {
            let alloc = resources::node_allocatable(&info.node, resource);
            let free = if alloc.is_infinite() {
                100.0
            } else {
                let used = resources::node_requested(info, resource);
                ((alloc - used) / alloc * 100.0).clamp(0.0, 100.0)
            };
            sum += free;
            count += 1.0;
        }
        // The incoming pod's own request is considered by ResourceFit's
        // filter, not here (upstream also scores free-at-placement).
        (sum / count).round() as i64
    }
}

/// `NodeAffinityPreferred`: sum of matched preferred-term weights, clamped
/// to 0..=100 (upstream normalizes per-node; a deterministic clamp keeps
/// golden output stable).
pub struct NodeAffinityPreferredScore;

impl Score for NodeAffinityPreferredScore {
    fn name(&self) -> &'static str {
        "NodeAffinityPreferred"
    }
    fn weight(&self) -> i64 {
        1
    }
    fn score(&self, pod: &Value, info: &NodeInfo, _snap: &Snapshot) -> i64 {
        affinity::node_affinity_preferred_weight(&info.node, pod).clamp(0, 100)
    }
}

/// `PodAntiAffinityPreferred`: `100 - 10 * conflicts`, floored at 0 — every
/// co-located soft-conflicting pod costs a decile.
pub struct PodAntiAffinityPreferredScore;

impl Score for PodAntiAffinityPreferredScore {
    fn name(&self) -> &'static str {
        "PodAntiAffinityPreferred"
    }
    fn weight(&self) -> i64 {
        1
    }
    fn score(&self, pod: &Value, info: &NodeInfo, _snap: &Snapshot) -> i64 {
        (100 - 10 * affinity::anti_affinity_penalty(pod, info)).clamp(0, 100)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Arc;

    fn node_info(name: &str, alloc: Value, placed: Vec<Value>) -> NodeInfo {
        NodeInfo {
            node: Arc::new(json!({
                "metadata": {"name": name},
                "status": {"allocatable": alloc}
            })),
            pods: placed.into_iter().map(Arc::new).collect(),
        }
    }

    fn snap_empty() -> Snapshot {
        Snapshot::default()
    }

    #[test]
    fn least_requested_prefers_the_emptier_node() {
        let busy = node_info(
            "busy",
            json!({"cpu": "4", "memory": "8Gi"}),
            vec![json!({"spec": {"nodeName": "busy", "containers": [
                {"name": "c", "resources": {"requests": {"cpu": "3", "memory": "7Gi"}}}
            ]}})],
        );
        let idle = node_info("idle", json!({"cpu": "4", "memory": "8Gi"}), vec![]);
        let pod = json!({"spec": {"containers": [{"name": "c"}]}});
        let snap = snap_empty();
        let busy_score = LeastRequestedScore.score(&pod, &busy, &snap);
        let idle_score = LeastRequestedScore.score(&pod, &idle, &snap);
        assert!(
            idle_score > busy_score,
            "idle {idle_score} vs busy {busy_score}"
        );
    }

    #[test]
    fn logical_node_scores_full() {
        let logical = NodeInfo {
            node: Arc::new(json!({"metadata": {"name": "l"}})),
            pods: vec![],
        };
        let snap = snap_empty();
        let pod = json!({"spec": {}});
        assert_eq!(LeastRequestedScore.score(&pod, &logical, &snap), 100);
    }

    #[test]
    fn preferred_affinity_adds_weight_bonus() {
        let node = Arc::new(json!({"metadata": {"name": "n", "labels": {"disk": "ssd"}}}));
        let info = NodeInfo { node, pods: vec![] };
        let snap = snap_empty();
        let pod = json!({"spec": {"affinity": {"nodeAffinity": {
            "preferredDuringSchedulingIgnoredDuringExecution": [
                {"weight": 50, "preference": {"matchExpressions": [
                    {"key": "disk", "operator": "In", "values": ["ssd"]}
                ]}}
            ]
        }}}});
        assert_eq!(NodeAffinityPreferredScore.score(&pod, &info, &snap), 50);
    }
}
