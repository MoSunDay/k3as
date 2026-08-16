//! Pure Kubernetes object builders/readers for the kubelet loops (TODO
//! **T4.2**). JSON-only (Q10): everything is `serde_json::Value` accessed
//! through JSON pointers; every function is total and side-effect free so
//! the reconcile core stays unit-testable.

use serde_json::{json, Value};

/// One container of a pod spec (the fields the CRI config needs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerSpec {
    pub name: String,
    pub image: String,
    pub command: Vec<String>,
    pub args: Vec<String>,
}

/// Pod identity + the fields the sync loop consumes. `marker` is the pod's
/// `resourceVersion` at observation time so status writes can skip no-op
/// pods.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PodView {
    pub key: String,
    pub namespace: String,
    pub name: String,
    pub uid: String,
    pub containers: Vec<ContainerSpec>,
    pub deleted: bool,
    pub marker: String,
}

/// `"namespace/name"` key; namespace defaults to `"default"`.
pub fn pod_key(pod: &Value) -> Option<String> {
    let name = pod.pointer("/metadata/name")?.as_str()?;
    let ns = pod
        .pointer("/metadata/namespace")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or("default");
    Some(format!("{ns}/{name}"))
}

/// The node this pod is scheduled on (`spec.nodeName`), if any.
pub fn pod_node(pod: &Value) -> Option<String> {
    pod.pointer("/spec/nodeName")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Stable pod uid; synthesized when the apiserver didn't assign one.
pub fn pod_uid(pod: &Value) -> String {
    pod.pointer("/metadata/uid")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("no-uid-{}", pod_key(pod).unwrap_or_default()))
}

/// True when `metadata.deletionTimestamp` is present (soft delete in flight).
pub fn pod_deleted(pod: &Value) -> bool {
    pod.pointer("/metadata/deletionTimestamp").is_some()
}

/// `spec.containers[]` as [`ContainerSpec`]s; malformed rows are skipped.
pub fn pod_containers(pod: &Value) -> Vec<ContainerSpec> {
    let Some(items) = pod.pointer("/spec/containers").and_then(Value::as_array) else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|c| {
            let name = c.pointer("/name").and_then(Value::as_str)?.to_string();
            if name.is_empty() {
                return None;
            }
            let image = c
                .pointer("/image")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let strings = |p: &str| {
                c.pointer(p)
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default()
            };
            Some(ContainerSpec {
                name,
                image,
                command: strings("/command"),
                args: strings("/args"),
            })
        })
        .collect()
}

/// Reduce one pod JSON object to the fields the sync loop needs. `None`
/// when the object has no usable name.
pub fn pod_view(pod: &Value) -> Option<PodView> {
    let key = pod_key(pod)?;
    let (namespace, name) = key.split_once('/')?;
    let namespace = namespace.to_string();
    let name = name.to_string();
    Some(PodView {
        key,
        namespace,
        name,
        uid: pod_uid(pod),
        containers: pod_containers(pod),
        deleted: pod_deleted(pod),
        marker: pod
            .pointer("/metadata/resourceVersion")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
    })
}

/// Minimal pod body for status writes (the `/status` route only reads
/// `.status`, so metadata beyond name/namespace is not needed).
pub fn pod_stub(view: &PodView) -> Value {
    json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {"name": view.name, "namespace": view.namespace},
    })
}

/// The Node object this kubelet registers (deterministic: `now` is passed
/// in, so tests can pin the timestamps).
pub fn node_object(name: &str, now: &str) -> Value {
    json!({
        "apiVersion": "v1",
        "kind": "Node",
        "metadata": {
            "name": name,
            "labels": {
                "kubernetes.io/hostname": name,
                "kubernetes.io/os": "linux",
                "kubernetes.io/arch": "amd64",
            },
        },
        "spec": {},
        "status": {
            "capacity": {"cpu": "4", "memory": "8Gi", "pods": "110"},
            "allocatable": {"cpu": "4", "memory": "8Gi", "pods": "110"},
            "conditions": [{
                "type": "Ready",
                "status": "True",
                "reason": "KubeletReady",
                "message": "init-pro kubelet ready",
                "lastHeartbeatTime": now,
                "lastTransitionTime": now,
            }],
            "addresses": [{"type": "InternalIP", "address": "127.0.0.1"}],
            "nodeInfo": {
                "kubeletVersion": "init-pro-0.2.0",
                "containerRuntimeVersion": "containerd://1.7.20",
                "operatingSystem": "linux",
                "architecture": "amd64",
            },
        },
    })
}

/// A fresh node-lease heartbeat object (`coordination.k8s.io/v1`).
pub fn new_lease(namespace: &str, name: &str, holder: &str, now: &str) -> Value {
    json!({
        "apiVersion": "coordination.k8s.io/v1",
        "kind": "Lease",
        "metadata": {"name": name, "namespace": namespace},
        "spec": {
            "holderIdentity": holder,
            "leaseDurationSeconds": 40,
            "renewTime": now,
        },
    })
}

/// Renew a lease: bump `spec.renewTime`, keep `metadata.resourceVersion`
/// for the CAS PUT.
pub fn renew_lease(lease: &Value, now: &str) -> Value {
    let mut next = lease.clone();
    if let Some(spec) = next.pointer_mut("/spec/renewTime") {
        *spec = Value::String(now.to_string());
    }
    next
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pod(ns: &str, name: &str, node: &str) -> Value {
        json!({
            "metadata": {"name": name, "namespace": ns, "uid": format!("uid-{name}"), "resourceVersion": "7"},
            "spec": {"nodeName": node, "containers": [
                {"name": "c1", "image": "img:1", "command": ["/bin/c1"], "args": ["--flag"]},
                {"name": "c2", "image": "img:2"},
            ]},
        })
    }

    #[test]
    fn key_node_uid_deleted() {
        let p = pod("ns1", "web", "n1");
        assert_eq!(pod_key(&p).as_deref(), Some("ns1/web"));
        assert_eq!(pod_node(&p).as_deref(), Some("n1"));
        assert_eq!(pod_uid(&p), "uid-web");
        assert!(!pod_deleted(&p));
        let mut d = p.clone();
        d["metadata"]["deletionTimestamp"] = json!("2026-08-16T00:00:00Z");
        assert!(pod_deleted(&d));
    }

    #[test]
    fn key_defaults_namespace_and_uid_is_synthesized() {
        let p = json!({"metadata": {"name": "solo"}, "spec": {}});
        assert_eq!(pod_key(&p).as_deref(), Some("default/solo"));
        assert_eq!(pod_uid(&p), "no-uid-default/solo");
        assert_eq!(pod_node(&p), None);
        assert_eq!(pod_key(&json!({})), None);
    }

    #[test]
    fn containers_are_parsed_tolerantly() {
        let cs = pod_containers(&pod("d", "p", "n"));
        assert_eq!(cs.len(), 2);
        assert_eq!(cs[0].name, "c1");
        assert_eq!(cs[0].image, "img:1");
        assert_eq!(cs[0].command, vec!["/bin/c1"]);
        assert_eq!(cs[0].args, vec!["--flag"]);
        assert_eq!(cs[1].args, Vec::<String>::new());
        // Nameless rows and non-string entries are skipped, not fatal.
        let messy = json!({"spec": {"containers": [
            {"image": "no-name"},
            {"name": "ok", "image": "i", "command": ["a", 42]},
        ]}});
        let cs = pod_containers(&messy);
        assert_eq!(cs.len(), 1);
        assert_eq!(cs[0].command, vec!["a"]);
        assert!(pod_containers(&json!({})).is_empty());
    }

    #[test]
    fn view_collects_all_fields() {
        let v = pod_view(&pod("ns1", "web", "n1")).unwrap();
        assert_eq!(v.key, "ns1/web");
        assert_eq!(v.namespace, "ns1");
        assert_eq!(v.name, "web");
        assert_eq!(v.uid, "uid-web");
        assert_eq!(v.containers.len(), 2);
        assert!(!v.deleted);
        assert_eq!(v.marker, "7");
        assert!(pod_view(&json!({})).is_none());
    }

    #[test]
    fn node_object_shape() {
        let n = node_object("n1", "2026-08-16T00:00:00Z");
        assert_eq!(n["apiVersion"], "v1");
        assert_eq!(n["kind"], "Node");
        assert_eq!(n["metadata"]["name"], "n1");
        assert_eq!(n["metadata"]["labels"]["kubernetes.io/hostname"], "n1");
        assert_eq!(n["status"]["capacity"]["pods"], "110");
        assert_eq!(n["status"]["conditions"][0]["type"], "Ready");
        assert_eq!(n["status"]["conditions"][0]["status"], "True");
        assert_eq!(
            n["status"]["conditions"][0]["lastHeartbeatTime"],
            "2026-08-16T00:00:00Z"
        );
        assert_eq!(
            n["status"]["addresses"][0],
            json!({"type": "InternalIP", "address": "127.0.0.1"})
        );
        assert_eq!(n["status"]["nodeInfo"]["kubeletVersion"], "init-pro-0.2.0");
    }

    #[test]
    fn lease_create_and_renew() {
        let l = new_lease("kube-node-lease", "n1", "holder-1", "2026-08-16T00:00:00Z");
        assert_eq!(l["apiVersion"], "coordination.k8s.io/v1");
        assert_eq!(l["kind"], "Lease");
        assert_eq!(l["spec"]["holderIdentity"], "holder-1");
        assert_eq!(l["spec"]["leaseDurationSeconds"], 40);
        assert_eq!(l["spec"]["renewTime"], "2026-08-16T00:00:00Z");
        let mut stored = l.clone();
        stored["metadata"]["resourceVersion"] = json!("42");
        let renewed = renew_lease(&stored, "2026-08-16T00:00:10Z");
        assert_eq!(renewed["spec"]["renewTime"], "2026-08-16T00:00:10Z");
        assert_eq!(renewed["metadata"]["resourceVersion"], "42");
    }
}
