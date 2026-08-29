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
    /// Non-terminal `init-pro.io/v1 Agent` objects (TODO A4) — informational
    /// only. Agents schedule through the SAME filter/score/binding loop as
    /// pods but do NOT consume node capacity in Phase A, so they are
    /// deliberately NOT merged into `pods`: the ResourceFit request sums and
    /// PodAntiAffinity pod counts stay byte-identical with the pod-only
    /// behaviour. (Agents carry no `/spec/containers[].resources.requests`
    /// today, so even a merged read would see zero requests — the explicit
    /// separation makes that accidental non-cost a documented contract.)
    pub agents: Vec<Arc<Value>>,
}

impl Snapshot {
    /// Assemble a snapshot from informer caches: nodes each with their bound
    /// pods, plus the full pod list (terminal pods excluded).
    pub fn build(pods: &[Arc<Value>], nodes: &[Arc<Value>], pvcs: &[Arc<Value>]) -> Self {
        Self::build_with_agents(pods, nodes, pvcs, &[])
    }

    /// [`Snapshot::build`] plus the `init-pro.io/v1 Agent` list (TODO A4);
    /// terminal agents are dropped exactly like terminal pods.
    pub fn build_with_agents(
        pods: &[Arc<Value>],
        nodes: &[Arc<Value>],
        pvcs: &[Arc<Value>],
        agents: &[Arc<Value>],
    ) -> Self {
        let live: Vec<Arc<Value>> = pods.iter().filter(|p| !is_terminal(p)).cloned().collect();
        let live_agents: Vec<Arc<Value>> = agents
            .iter()
            .filter(|a| !is_terminal(a))
            .cloned()
            .collect();
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
            agents: live_agents,
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

/// Pending agent (TODO A4) = the [`is_pending`] gate applied to an
/// `init-pro.io/v1 Agent`: no `spec.nodeName`, not terminating, not terminal
/// (`status.phase` is neither `Succeeded` nor `Failed` — a missing or empty
/// phase counts as schedulable, fresh agents carry no status yet), and
/// `spec.schedulerName` empty-or-ours. The same camelCase `/spec/*` fields
/// (`nodeSelector`, `affinity`, `tolerations`, `nodeName`, `schedulerName`)
/// feed the pod plugins unchanged; the Agent's top-level `/spec/resources`
/// is not read by ResourceFit in Phase A (zero requests -> pass).
pub fn is_agent_pending(agent: &Value, scheduler_name: &str) -> bool {
    if agent
        .pointer("/spec/nodeName")
        .and_then(|v| v.as_str())
        .map(|n| !n.is_empty())
        .unwrap_or(false)
    {
        return false;
    }
    if agent
        .pointer("/metadata/deletionTimestamp")
        .map(|v| !v.is_null())
        .unwrap_or(false)
    {
        return false;
    }
    let ours = agent
        .pointer("/spec/schedulerName")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if !(ours.is_empty() || ours == scheduler_name) {
        return false;
    }
    !matches!(
        agent.pointer("/status/phase").and_then(|v| v.as_str()),
        Some("Succeeded") | Some("Failed")
    )
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

    fn agent(node: Option<&str>) -> Value {
        json!({
            "apiVersion": "init-pro.io/v1",
            "kind": "Agent",
            "metadata": {"name": "a1", "namespace": "default"},
            "spec": match node { Some(n) => json!({"nodeName": n}), None => json!({}) }
        })
    }

    #[test]
    fn is_agent_pending_requires_no_nodename_and_our_scheduler() {
        assert!(is_agent_pending(&agent(None), "default-scheduler"));
        assert!(!is_agent_pending(&agent(Some("a")), "default-scheduler"));
        let mut named = agent(None);
        named["spec"]["schedulerName"] = json!("custom");
        assert!(!is_agent_pending(&named, "default-scheduler"));
        let mut ours = agent(None);
        ours["spec"]["schedulerName"] = json!("default-scheduler");
        assert!(is_agent_pending(&ours, "default-scheduler"));
    }

    #[test]
    fn is_agent_pending_skips_terminating_and_terminal_phases() {
        let mut gone = agent(None);
        gone["metadata"]["deletionTimestamp"] = json!("2026-01-01T00:00:00Z");
        assert!(!is_agent_pending(&gone, "default-scheduler"));
        let mut done = agent(None);
        done["status"]["phase"] = json!("Succeeded");
        assert!(!is_agent_pending(&done, "default-scheduler"));
        let mut failed = agent(None);
        failed["status"]["phase"] = json!("Failed");
        assert!(!is_agent_pending(&failed, "default-scheduler"));
    }

    #[test]
    fn is_agent_pending_treats_missing_or_empty_phase_as_schedulable() {
        // No status block at all (fresh agent).
        assert!(is_agent_pending(&agent(None), "default-scheduler"));
        // Explicitly empty phase is not terminal either.
        let mut blank = agent(None);
        blank["status"]["phase"] = json!("");
        assert!(is_agent_pending(&blank, "default-scheduler"));
        // An empty nodeName is still "unbound".
        let mut empty_node = agent(None);
        empty_node["spec"]["nodeName"] = json!("");
        assert!(is_agent_pending(&empty_node, "default-scheduler"));
    }

    #[test]
    fn build_with_agents_keeps_agents_out_of_the_pod_set() {
        let nodes = vec![Arc::new(json!({"metadata": {"name": "a"}}))];
        let pods = vec![Arc::new(pod(Some("a")))];
        let agents = vec![
            Arc::new(agent(None)),
            Arc::new(json!({
                "apiVersion": "init-pro.io/v1",
                "kind": "Agent",
                "metadata": {"name": "done"},
                "status": {"phase": "Succeeded"}
            })),
        ];
        let snap = Snapshot::build_with_agents(&pods, &nodes, &[], &agents);
        assert_eq!(snap.pods.len(), 1);
        assert_eq!(snap.agents.len(), 1);
        // Terminal agents are dropped, and `build` (no agents) stays empty.
        assert_eq!(snap.agents[0].pointer("/metadata/name"), Some(&json!("a1")));
        assert_eq!(Snapshot::build(&pods, &nodes, &[]).agents.len(), 0);
    }
}
