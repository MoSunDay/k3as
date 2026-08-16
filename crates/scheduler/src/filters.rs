//! Default filter plugins (T3.2): pure `(pod, node, snapshot) -> Verdict`
//! functions in the upstream framework order. Every plugin is a unit struct
//! implementing [`Filter`]; matching helpers live in [`crate::affinity`] /
//! [`crate::resources`].

use serde_json::Value;

use crate::affinity;
use crate::plugin::{node_name, Filter, NodeInfo, Snapshot, Verdict};
use crate::resources;

/// `NodeName`: a pod pre-assigned via `spec.nodeName` passes only that node
/// (the scheduler normally never sees such pods; this guards direct-API
/// stragglers and keeps the filter registry upstream-complete).
pub struct NodeNameFilter;

impl Filter for NodeNameFilter {
    fn name(&self) -> &'static str {
        "NodeName"
    }
    fn filter(&self, pod: &Value, info: &NodeInfo, _snap: &Snapshot) -> Verdict {
        match pod.pointer("/spec/nodeName").and_then(|v| v.as_str()) {
            Some(want) if !want.is_empty() => {
                if node_name(&info.node).as_deref() == Some(want) {
                    Verdict::Pass
                } else {
                    Verdict::reject(
                        self.name(),
                        format!("node(s) didn't match the requested node name {want}"),
                    )
                }
            }
            _ => Verdict::Pass,
        }
    }
}

/// `NodeUnschedulable`: cordoned nodes (`spec.unschedulable: true`) reject
/// pods unless they tolerate the `node.kubernetes.io/unschedulable` taint.
pub struct NodeUnschedulableFilter;

/// The taint `kubectl cordon`/drain reason with (k3s parity).
pub const UNSCHEDULABLE_TAINT_KEY: &str = "node.kubernetes.io/unschedulable";

impl Filter for NodeUnschedulableFilter {
    fn name(&self) -> &'static str {
        "NodeUnschedulable"
    }
    fn filter(&self, pod: &Value, info: &NodeInfo, _snap: &Snapshot) -> Verdict {
        let cordoned = info
            .node
            .pointer("/spec/unschedulable")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if !cordoned {
            return Verdict::Pass;
        }
        let tolerates = pod
            .pointer("/spec/tolerations")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter().any(|t| {
                    let key = t.get("key").and_then(|v| v.as_str()).unwrap_or("");
                    key == UNSCHEDULABLE_TAINT_KEY
                        || key.is_empty()
                            && t.get("operator").and_then(|v| v.as_str()) == Some("Exists")
                })
            })
            .unwrap_or(false);
        if tolerates {
            Verdict::Pass
        } else {
            Verdict::reject(self.name(), "node(s) were unschedulable")
        }
    }
}

/// `TaintToleration`: every `NoSchedule`/`NoExecute` taint must be tolerated.
pub struct TaintTolerationFilter;

impl Filter for TaintTolerationFilter {
    fn name(&self) -> &'static str {
        "TaintToleration"
    }
    fn filter(&self, pod: &Value, info: &NodeInfo, _snap: &Snapshot) -> Verdict {
        if affinity::tolerates_all(pod, &info.node) {
            Verdict::Pass
        } else {
            Verdict::reject(self.name(), "node(s) had untolerated taints")
        }
    }
}

/// `NodeAffinity`: required nodeAffinity terms + `spec.nodeSelector` map.
pub struct NodeAffinityFilter;

impl Filter for NodeAffinityFilter {
    fn name(&self) -> &'static str {
        "NodeAffinity"
    }
    fn filter(&self, pod: &Value, info: &NodeInfo, _snap: &Snapshot) -> Verdict {
        if let Some(sel) = pod.pointer("/spec/nodeSelector") {
            if !affinity::node_selector_matches(&info.node, sel) {
                return Verdict::reject(self.name(), "node(s) didn't match pod's node selector");
            }
        }
        if !affinity::node_affinity_required_matches(&info.node, pod) {
            return Verdict::reject(self.name(), "node(s) didn't match pod node affinity");
        }
        Verdict::Pass
    }
}

/// `PodAntiAffinity`: required anti-affinity terms block conflicting nodes.
pub struct PodAntiAffinityFilter;

impl Filter for PodAntiAffinityFilter {
    fn name(&self) -> &'static str {
        "PodAntiAffinity"
    }
    fn filter(&self, pod: &Value, info: &NodeInfo, snap: &Snapshot) -> Verdict {
        if affinity::anti_affinity_violated(pod, info, snap) {
            Verdict::reject(
                self.name(),
                "node(s) didn't satisfy pod anti-affinity rules",
            )
        } else {
            Verdict::Pass
        }
    }
}

/// `ResourceFit`: pod requests must fit the node's remaining allocatable.
/// A logical node (no capacity) is unbounded (**Q23**).
pub struct ResourceFitFilter;

impl Filter for ResourceFitFilter {
    fn name(&self) -> &'static str {
        "ResourceFit"
    }
    fn filter(&self, pod: &Value, info: &NodeInfo, _snap: &Snapshot) -> Verdict {
        if resources::fits(pod, info) {
            Verdict::Pass
        } else {
            Verdict::reject(self.name(), "Insufficient cpu/memory")
        }
    }
}

/// `VolumeBinding` (v1 passthrough): no PV controller exists until T6.2, so
/// PVCs stay Pending (Q22) and pods must not be blocked on them. The filter
/// only fails a pod whose PVC is already bound to a PV with a
/// `nodeAffinity` naming a different node — the one case the storage layer
/// can express today.
pub struct VolumeBindingFilter;

impl Filter for VolumeBindingFilter {
    fn name(&self) -> &'static str {
        "VolumeBinding"
    }
    fn filter(&self, pod: &Value, info: &NodeInfo, snap: &Snapshot) -> Verdict {
        let Some(volumes) = pod.pointer("/spec/volumes").and_then(|v| v.as_array()) else {
            return Verdict::Pass;
        };
        let me = node_name(&info.node);
        for vol in volumes {
            let Some(claim) = vol
                .pointer("/persistentVolumeClaim/claimName")
                .and_then(|v| v.as_str())
            else {
                continue;
            };
            let Some(pvc) = snap
                .pvcs
                .iter()
                .find(|p| p.pointer("/metadata/name").and_then(|v| v.as_str()) == Some(claim))
            else {
                continue; // unknown claim: admission concern, not scheduling
            };
            if pvc
                .pointer("/spec/volumeName")
                .and_then(|v| v.as_str())
                .is_none()
            {
                continue; // unbound (always, until T6.2) -> pass
            }
            // The PV object is not served in v1; a PVC that records its own
            // node constraint (init-pro extension `spec.nodeAffinity`) is
            // honoured so the seam is testable end-to-end.
            let pinned = pvc
                .pointer("/spec/nodeAffinity/required/nodeSelectorTerms")
                .and_then(|v| v.as_array())
                .map(|terms| {
                    terms.iter().any(|t| {
                        t.pointer("/matchFields/nodeName")
                            .or_else(|| t.pointer("/matchExpressions/0/values/0"))
                            .and_then(|v| v.as_str())
                            .map(|n| Some(n) == me.as_deref())
                            .unwrap_or(true)
                    })
                });
            if pinned == Some(false) {
                return Verdict::reject(self.name(), "node(s) didn't match persistent volume(s)");
            }
        }
        Verdict::Pass
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Arc;

    fn snap_of(nodes: &[Value], pods: &[Value]) -> Snapshot {
        let n: Vec<_> = nodes.iter().map(|v| Arc::new(v.clone())).collect();
        let p: Vec<_> = pods.iter().map(|v| Arc::new(v.clone())).collect();
        Snapshot::build(&p, &n, &[])
    }

    fn info_of(node: &Value, snap: &Snapshot) -> NodeInfo {
        snap.node(
            node.pointer("/metadata/name")
                .and_then(|v| v.as_str())
                .unwrap(),
        )
        .unwrap()
        .clone()
    }

    #[test]
    fn cordon_rejects_unless_tolerated() {
        let node = json!({"metadata": {"name": "a"}, "spec": {"unschedulable": true}});
        let snap = snap_of(std::slice::from_ref(&node), &[]);
        let info = info_of(&node, &snap);
        let plain = json!({"metadata": {"name": "p"}, "spec": {}});
        assert!(!matches!(
            NodeUnschedulableFilter.filter(&plain, &info, &snap),
            Verdict::Pass
        ));
        let tolerated = json!({"spec": {"tolerations": [
            {"key": UNSCHEDULABLE_TAINT_KEY, "operator": "Exists"}
        ]}});
        assert!(matches!(
            NodeUnschedulableFilter.filter(&tolerated, &info, &snap),
            Verdict::Pass
        ));
    }

    #[test]
    fn node_selector_rejects_mismatch() {
        let a = json!({"metadata": {"name": "a", "labels": {"disk": "ssd"}}});
        let b = json!({"metadata": {"name": "b"}});
        let snap = snap_of(&[a.clone(), b.clone()], &[]);
        let pod = json!({"metadata": {"name": "p"}, "spec": {"nodeSelector": {"disk": "ssd"}}});
        assert!(matches!(
            NodeAffinityFilter.filter(&pod, &info_of(&a, &snap), &snap),
            Verdict::Pass
        ));
        assert!(matches!(
            NodeAffinityFilter.filter(&pod, &info_of(&b, &snap), &snap),
            Verdict::Reject { .. }
        ));
    }

    #[test]
    fn full_registry_agrees_on_golden_shape() {
        // The G22 shape: labeled node vs plain node, nodeSelector pod.
        let a = json!({"metadata": {"name": "a", "labels": {"disk": "ssd"}}, "status": {"allocatable": {"cpu": "4", "memory": "8Gi"}}});
        let b = json!({"metadata": {"name": "b"}});
        let snap = snap_of(&[a.clone(), b.clone()], &[]);
        let pod = json!({"metadata": {"name": "p"}, "spec": {
            "nodeSelector": {"disk": "ssd"},
            "containers": [{"name": "c", "image": "i", "resources": {"requests": {"cpu": "100m"}}}]
        }});
        let mut passed_a = true;
        let mut passed_b = true;
        for f in crate::plugin::default_filters() {
            if !matches!(f.filter(&pod, &info_of(&a, &snap), &snap), Verdict::Pass) {
                passed_a = false;
            }
            if !matches!(f.filter(&pod, &info_of(&b, &snap), &snap), Verdict::Pass) {
                passed_b = false;
            }
        }
        assert!(passed_a, "labeled node must pass all filters");
        assert!(!passed_b, "unlabeled node must be rejected by NodeAffinity");
    }
}
