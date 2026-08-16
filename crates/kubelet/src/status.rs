//! Pod status construction for the `/status` write path (TODO **T4.2**).
//!
//! Scope A: phase is `Running` only when the sandbox is READY and every
//! spec container is RUNNING; everything else is `Pending`. Timestamps are
//! wall-clock strings passed in, and [`status_semantically_eq`] ignores
//! them so the sync loop doesn't spam identical PUTs every cycle.

use serde_json::{json, Value};

use crate::objects::PodView;
use crate::sync::Snapshot;

/// Build the `.status` subtree for one pod from the observed CRI state.
pub fn build_pod_status(pod: &PodView, snap: &Snapshot, now: &str) -> Value {
    let ready_sandbox = snap
        .sandboxes
        .iter()
        .find(|sb| sb.uid == pod.uid && sb.state == "SANDBOX_READY");
    let sandbox_ready = ready_sandbox.is_some();
    let mut statuses: Vec<Value> = Vec::new();
    let mut all_running = !pod.containers.is_empty();
    for spec in &pod.containers {
        // Highest-attempt observation wins (latest incarnation of the name).
        let observed = snap
            .containers
            .iter()
            .filter(|c| pod_owns(pod, c) && c.name == spec.name)
            .max_by_key(|c| c.attempt);
        let Some(c) = observed else {
            all_running = false;
            statuses.push(json!({
                "name": spec.name,
                "ready": false,
                "restartCount": 0,
                "image": spec.image,
                "state": {"waiting": {"reason": "ContainerCreating"}},
            }));
            continue;
        };
        let running = c.state == "CONTAINER_RUNNING";
        all_running = all_running && running;
        let state = if running {
            json!({"running": {"startedAt": now}})
        } else if c.state == "CONTAINER_EXITED" {
            json!({"terminated": {"exitCode": 0, "reason": "Completed", "finishedAt": now}})
        } else {
            json!({"terminated": {"exitCode": 137, "reason": "Error", "finishedAt": now}})
        };
        statuses.push(json!({
            "name": spec.name,
            "ready": running,
            "restartCount": c.attempt,
            "image": spec.image,
            "containerID": format!("cri://{}", c.id),
            "state": state,
        }));
    }
    let phase = if sandbox_ready && all_running {
        "Running"
    } else {
        "Pending"
    };
    let pod_ip = ready_sandbox.and_then(|sb| sb.ip.clone());
    let mut status = json!({
        "phase": phase,
        "conditions": [
            {
                "type": "PodScheduled",
                "status": "True",
                "reason": "Scheduler",
                "message": "assigned to this node",
                "lastTransitionTime": now,
            },
            {
                "type": "Ready",
                "status": if phase == "Running" { "True" } else { "False" },
                "reason": "ContainersReady",
                "message": if phase == "Running" { "all containers running" } else { "not all containers running" },
                "lastTransitionTime": now,
            },
        ],
        "containerStatuses": statuses,
    });
    if let Some(ip) = &pod_ip {
        // hostIP: single-node clusters advertise loopback (the agent and the
        // pod sandbox share one host); mirrored in objects::node_object.
        status["podIP"] = json!(ip);
        status["podIPs"] = json!([{ "ip": ip }]);
        status["hostIP"] = json!("127.0.0.1");
    }
    status
}

/// True when the container's labels mark it as belonging to `pod`.
fn pod_owns(pod: &PodView, c: &crate::cri_backend::ContainerView) -> bool {
    use runtime::cri_json::{LABEL_POD_NAME, LABEL_POD_NAMESPACE, LABEL_POD_UID};
    let uid = c.labels.get(LABEL_POD_UID).map(String::as_str);
    if let Some(u) = uid {
        return u == pod.uid;
    }
    let ns = c.labels.get(LABEL_POD_NAMESPACE).map(String::as_str);
    let name = c.labels.get(LABEL_POD_NAME).map(String::as_str);
    ns == Some(pod.namespace.as_str()) && name == Some(pod.name.as_str())
}

/// Semantic equality for status writes: phase + condition outcomes +
/// container identity/state, IGNORING timestamps (`startedAt`,
/// `finishedAt`, `lastTransitionTime`) so unchanged pods skip the PUT.
pub fn status_semantically_eq(a: &Value, b: &Value) -> bool {
    semantic_key(a) == semantic_key(b)
}

/// (phase, sorted conditions, container outcomes, podIP) — the semantic
/// status core.
type SemanticKey = (
    String,
    Vec<(String, String, String)>,
    Vec<(String, u64, String)>,
    String,
);

fn semantic_key(v: &Value) -> SemanticKey {
    let phase = v
        .pointer("/phase")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let mut conditions: Vec<(String, String, String)> = v
        .pointer("/conditions")
        .and_then(Value::as_array)
        .map(|cs| {
            cs.iter()
                .map(|c| {
                    let g = |p: &str| {
                        c.pointer(p)
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string()
                    };
                    (g("/type"), g("/status"), g("/reason"))
                })
                .collect()
        })
        .unwrap_or_default();
    conditions.sort();
    let mut containers: Vec<(String, u64, String)> = v
        .pointer("/containerStatuses")
        .and_then(Value::as_array)
        .map(|cs| {
            cs.iter()
                .map(|c| {
                    let name = c
                        .pointer("/name")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let restarts = c
                        .pointer("/restartCount")
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    // State shape without timestamps: which branch + its
                    // non-time fields (exitCode/reason for terminated).
                    let state = c
                        .pointer("/state")
                        .and_then(Value::as_object)
                        .and_then(|o| o.keys().next())
                        .cloned()
                        .unwrap_or_default();
                    let detail = match state.as_str() {
                        "terminated" => {
                            let code = c
                                .pointer("/state/terminated/exitCode")
                                .and_then(Value::as_i64)
                                .unwrap_or(-1);
                            let reason = c
                                .pointer("/state/terminated/reason")
                                .and_then(Value::as_str)
                                .unwrap_or("");
                            format!("terminated:{code}:{reason}")
                        }
                        other => other.to_string(),
                    };
                    (name, restarts, detail)
                })
                .collect()
        })
        .unwrap_or_default();
    containers.sort();
    let pod_ip = v
        .pointer("/podIP")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    (phase, conditions, containers, pod_ip)
}

/// Clone `pod` with `.status` replaced — the body the `/status` route PUTs
/// (the route only reads `.status`; stored metadata/spec survive).
pub fn merge_pod_for_status(pod: &Value, status: &Value) -> Value {
    let mut merged = pod.clone();
    if let Some(obj) = merged.as_object_mut() {
        obj.insert("status".to_string(), status.clone());
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cri_backend::{ContainerView, SandboxView};
    use crate::objects::pod_view;
    use runtime::cri_json::container_labels;
    use std::collections::BTreeMap;

    fn pod() -> PodView {
        pod_view(&json!({
            "metadata": {"name": "web", "namespace": "default", "uid": "u1"},
            "spec": {"containers": [{"name": "c0", "image": "img:1"}]},
        }))
        .unwrap()
    }

    fn snap(state: &str) -> Snapshot {
        Snapshot {
            images: vec![],
            sandboxes: vec![SandboxView {
                id: "sb1".into(),
                state: "SANDBOX_READY".into(),
                labels: BTreeMap::new(),
                name: "web".into(),
                namespace: "default".into(),
                uid: "u1".into(),
                ip: None,
            }],
            containers: vec![ContainerView {
                id: "cid1".into(),
                sandbox_id: "sb1".into(),
                state: state.into(),
                name: "c0".into(),
                attempt: 0,
                labels: container_labels("web", "default", "u1", "c0"),
            }],
        }
    }

    #[test]
    fn running_pod_status_shape() {
        let s = build_pod_status(&pod(), &snap("CONTAINER_RUNNING"), "2026-08-16T00:00:00Z");
        assert_eq!(s["phase"], "Running");
        assert_eq!(s["conditions"][0]["type"], "PodScheduled");
        assert_eq!(s["conditions"][1]["type"], "Ready");
        assert_eq!(s["conditions"][1]["status"], "True");
        assert_eq!(s["containerStatuses"][0]["ready"], true);
        assert_eq!(s["containerStatuses"][0]["containerID"], "cri://cid1");
        assert_eq!(s["containerStatuses"][0]["restartCount"], 0);
        assert_eq!(
            s["containerStatuses"][0]["state"]["running"]["startedAt"],
            "2026-08-16T00:00:00Z"
        );
    }

    #[test]
    fn exited_container_is_pending_and_terminated() {
        let s = build_pod_status(&pod(), &snap("CONTAINER_EXITED"), "T");
        assert_eq!(s["phase"], "Pending");
        assert_eq!(s["conditions"][1]["status"], "False");
        assert_eq!(
            s["containerStatuses"][0]["state"]["terminated"]["exitCode"],
            0
        );
        assert_eq!(
            s["containerStatuses"][0]["state"]["terminated"]["reason"],
            "Completed"
        );
        // Non-exit unknown states map to error termination.
        let s = build_pod_status(&pod(), &snap("CONTAINER_UNKNOWN"), "T");
        assert_eq!(
            s["containerStatuses"][0]["state"]["terminated"]["exitCode"],
            137
        );
        assert_eq!(
            s["containerStatuses"][0]["state"]["terminated"]["reason"],
            "Error"
        );
    }

    #[test]
    fn missing_container_reports_waiting() {
        let mut s0 = snap("CONTAINER_RUNNING");
        s0.containers.clear();
        let s = build_pod_status(&pod(), &s0, "T");
        assert_eq!(s["phase"], "Pending");
        assert_eq!(
            s["containerStatuses"][0]["state"]["waiting"]["reason"],
            "ContainerCreating"
        );
    }

    #[test]
    fn semantic_eq_ignores_timestamps_only() {
        let a = build_pod_status(&pod(), &snap("CONTAINER_RUNNING"), "T1");
        let b = build_pod_status(&pod(), &snap("CONTAINER_RUNNING"), "T2");
        assert!(status_semantically_eq(&a, &b), "timestamps must not matter");
        let c = build_pod_status(&pod(), &snap("CONTAINER_EXITED"), "T1");
        assert!(!status_semantically_eq(&a, &c), "state change must matter");
        let mut d = a.clone();
        d["containerStatuses"][0]["restartCount"] = json!(1);
        assert!(!status_semantically_eq(&a, &d), "restartCount must matter");
        let mut e = a.clone();
        e["phase"] = json!("Pending");
        assert!(!status_semantically_eq(&a, &e), "phase must matter");
    }

    #[test]
    fn ready_sandbox_ip_surfaces_as_pod_ip() {
        let mut snap = snap("CONTAINER_RUNNING");
        snap.sandboxes[0].ip = Some("10.42.0.10".into());
        let s = build_pod_status(&pod(), &snap, "T");
        assert_eq!(s["phase"], "Running");
        assert_eq!(s["podIP"], "10.42.0.10");
        assert_eq!(s["podIPs"][0]["ip"], "10.42.0.10");
        assert_eq!(s["hostIP"], "127.0.0.1");
    }

    #[test]
    fn ready_sandbox_without_ip_omits_pod_ip() {
        let s = build_pod_status(&pod(), &snap("CONTAINER_RUNNING"), "T");
        assert_eq!(s["phase"], "Running");
        assert!(s.get("podIP").is_none(), "no ip -> no podIP key: {s}");
        assert!(s.get("podIPs").is_none());
        assert!(s.get("hostIP").is_none());
    }

    #[test]
    fn semantic_eq_detects_pod_ip_change() {
        let snap = snap("CONTAINER_RUNNING");
        let without = build_pod_status(&pod(), &snap, "T");
        let mut with_ip = snap.clone();
        with_ip.sandboxes[0].ip = Some("10.42.0.10".into());
        let with = build_pod_status(&pod(), &with_ip, "T");
        assert!(
            !status_semantically_eq(&without, &with),
            "late-arriving podIP must trigger a status re-write"
        );
        let same = build_pod_status(&pod(), &with_ip, "T2");
        assert!(status_semantically_eq(&with, &same));
    }

    #[test]
    fn merge_replaces_status_only() {
        let pod = json!({"metadata": {"name": "web"}, "spec": {"x": 1}});
        let merged = merge_pod_for_status(&pod, &json!({"phase": "Running"}));
        assert_eq!(merged["spec"]["x"], 1);
        assert_eq!(merged["status"]["phase"], "Running");
        assert!(pod.get("status").is_none(), "input untouched");
    }
}
