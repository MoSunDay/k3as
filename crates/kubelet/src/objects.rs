//! Pure Kubernetes object builders/readers for the kubelet loops (TODO
//! **T4.2**). JSON-only (Q10): everything is `serde_json::Value` accessed
//! through JSON pointers; every function is total and side-effect free so
//! the reconcile core stays unit-testable.

use serde_json::{json, Value};

use crate::probes::ProbeSpec;

/// One container of a pod spec (the fields the CRI config needs). Probes
/// are Sprint 20 S1: `httpGet` only; `None` when the container declares
/// none. `volume_mounts` is Sprint 20 S2 (T4.2 Scope B).
/// `image_pull_policy` is Sprint 20 S3: the RESOLVED value ("Always" /
/// "IfNotPresent" / "Never") — absent fields are defaulted at parse time
/// following upstream defaulting (see [`resolve_image_pull_policy`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerSpec {
    pub name: String,
    pub image: String,
    pub command: Vec<String>,
    pub args: Vec<String>,
    pub startup_probe: Option<ProbeSpec>,
    pub readiness_probe: Option<ProbeSpec>,
    pub liveness_probe: Option<ProbeSpec>,
    pub volume_mounts: Vec<VolumeMount>,
    pub image_pull_policy: String,
}

/// One `spec.containers[].volumeMounts[]` entry (Sprint 20 S2). `mountPath`
/// is required; `readOnly` defaults to false. subPath/mountPropagation are
/// deferred (v1 mounts the whole volume dir at `mountPath`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeMount {
    pub name: String,
    pub mount_path: String,
    pub read_only: bool,
}

/// One `spec.volumes[]` entry (Sprint 20 S2). Only emptyDir, configMap, and
/// secret are supported; optional/defaultMode/subPath/CSI volumes are
/// deferred — unsupported entries parse to `None` and are skipped by the
/// sync loop (the CRI create then fails and the cycle retries).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeSpec {
    pub name: String,
    pub kind: VolumeKind,
}

/// Volume kind: configMap references `spec.volumes[].configMap.name`,
/// secret references `spec.volumes[].secret.secretName`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VolumeKind {
    EmptyDir,
    ConfigMap { name: String },
    Secret { name: String },
}

/// Pod identity + the fields the sync loop consumes. `marker` is the pod's
/// `resourceVersion` at observation time so status writes can skip no-op
/// pods. `restart_policy` is `spec.restartPolicy`, defaulting to "Always".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PodView {
    pub key: String,
    pub namespace: String,
    pub name: String,
    pub uid: String,
    pub containers: Vec<ContainerSpec>,
    pub volumes: Vec<VolumeSpec>,
    pub deleted: bool,
    pub marker: String,
    pub restart_policy: String,
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

/// The upstream default image pull policy for an image reference without an
/// explicit field (k8s `pkg/apis/core/v1` `SetDefaults_Container`): no tag
/// or tag `latest` -> Always, any other tag -> IfNotPresent. A registry
/// port (`localhost:5000/img`) is not a tag; digest refs
/// (`img@sha256:...`) are immutable and default to IfNotPresent (modern
/// upstream behavior).
fn default_image_pull_policy(image: &str) -> String {
    let base = image.rsplit('/').next().unwrap_or(image);
    if base.contains('@') {
        return "IfNotPresent".to_string();
    }
    match base
        .rsplit_once(':')
        .map(|(_, tag)| tag)
        .filter(|t| !t.is_empty())
    {
        Some(tag) if tag != "latest" => "IfNotPresent".to_string(),
        _ => "Always".to_string(),
    }
}

/// Sprint 20 S3: the resolved image pull policy for one container. Explicit
/// `Always`/`IfNotPresent`/`Never` pass through; anything else (upstream
/// would reject it at admission) falls back to [`default_image_pull_policy`].
/// `imagePullSecrets` is explicitly DEFERRED (Sprint 20 / T4.2 card):
/// pulls are unauthenticated until a later sprint.
fn resolve_image_pull_policy(image: &str, explicit: Option<&str>) -> String {
    match explicit {
        Some(p) if matches!(p, "Always" | "IfNotPresent" | "Never") => p.to_string(),
        _ => default_image_pull_policy(image),
    }
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
            let probe = |p: &str| c.pointer(p).and_then(ProbeSpec::from_json);
            let image_pull_policy = resolve_image_pull_policy(
                &image,
                c.pointer("/imagePullPolicy").and_then(Value::as_str),
            );
            Some(ContainerSpec {
                name,
                image,
                command: strings("/command"),
                args: strings("/args"),
                startup_probe: probe("/startupProbe"),
                readiness_probe: probe("/readinessProbe"),
                liveness_probe: probe("/livenessProbe"),
                volume_mounts: container_volume_mounts(c),
                image_pull_policy,
            })
        })
        .collect()
}

/// `spec.containers[].volumeMounts[]` (Sprint 20 S2): rows without a name
/// or mountPath are skipped, never fatal.
fn container_volume_mounts(c: &Value) -> Vec<VolumeMount> {
    c.pointer("/volumeMounts")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|m| {
                    let name = m.pointer("/name").and_then(Value::as_str)?;
                    let mount_path = m.pointer("/mountPath").and_then(Value::as_str)?;
                    if name.is_empty() || mount_path.is_empty() {
                        return None;
                    }
                    Some(VolumeMount {
                        name: name.to_string(),
                        mount_path: mount_path.to_string(),
                        read_only: m
                            .pointer("/readOnly")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// `spec.volumes[]` (Sprint 20 S2): only emptyDir/configMap/secret kinds;
/// nameless or unsupported entries are skipped, never fatal.
pub fn pod_volumes(pod: &Value) -> Vec<VolumeSpec> {
    let Some(items) = pod.pointer("/spec/volumes").and_then(Value::as_array) else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|v| {
            let name = v.pointer("/name").and_then(Value::as_str)?;
            if name.is_empty() {
                return None;
            }
            let kind = if v.pointer("/emptyDir").is_some() {
                VolumeKind::EmptyDir
            } else if let Some(cn) = v
                .pointer("/configMap/name")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                VolumeKind::ConfigMap {
                    name: cn.to_string(),
                }
            } else if let Some(sn) = v
                .pointer("/secret/secretName")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                VolumeKind::Secret {
                    name: sn.to_string(),
                }
            } else {
                return None;
            };
            Some(VolumeSpec {
                name: name.to_string(),
                kind,
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
        volumes: pod_volumes(pod),
        deleted: pod_deleted(pod),
        marker: pod
            .pointer("/metadata/resourceVersion")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        restart_policy: pod
            .pointer("/spec/restartPolicy")
            .and_then(Value::as_str)
            .filter(|p| matches!(*p, "Always" | "OnFailure" | "Never"))
            .unwrap_or("Always")
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
///
/// T4.2 Scope C (Q31): `status.addresses[InternalIP]` carries
/// [`crate::runner::KubeletConfig::node_ip`] and the metadata annotation
/// `init-pro.io/kubelet-port` carries the local log/exec listener port, so
/// the apiserver relay can dial `ip:port` without extra discovery.
pub fn node_object(name: &str, node_ip: &str, kubelet_port: u16, now: &str) -> Value {
    json!({
        "apiVersion": "v1",
        "kind": "Node",
        "metadata": {
            "name": name,
            "annotations": {
                "init-pro.io/kubelet-port": kubelet_port.to_string(),
            },
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
            "addresses": [{"type": "InternalIP", "address": node_ip}],
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
    fn image_pull_policy_is_resolved_at_parse_time() {
        // Sprint 20 S3 (T4.2 Scope B): upstream defaulting applied when the
        // field is absent — no tag or `latest` -> Always, any other tag ->
        // IfNotPresent; explicit values pass through untouched.
        let p = json!({"metadata": {"name": "p", "namespace": "d"}, "spec": {"containers": [
            {"name": "no-tag", "image": "nginx"},
            {"name": "latest", "image": "nginx:latest"},
            {"name": "pinned", "image": "nginx:1.27"},
            {"name": "explicit-never", "image": "nginx:1.27", "imagePullPolicy": "Never"},
            {"name": "explicit-ifnp", "image": "nginx", "imagePullPolicy": "IfNotPresent"},
            {"name": "explicit-always", "image": "nginx:1.27", "imagePullPolicy": "Always"},
            {"name": "registry-port", "image": "localhost:5000/img"},
            {"name": "digest", "image": "img@sha256:abc"},
        ]}});
        let cs = pod_containers(&p);
        let pol = |n: &str| {
            cs.iter()
                .find(|c| c.name == n)
                .unwrap_or_else(|| panic!("container {n}"))
                .image_pull_policy
                .as_str()
        };
        assert_eq!(pol("no-tag"), "Always", "no tag defaults to Always");
        assert_eq!(pol("latest"), "Always", "latest tag defaults to Always");
        assert_eq!(
            pol("pinned"),
            "IfNotPresent",
            "pinned tag defaults to IfNotPresent"
        );
        assert_eq!(pol("explicit-never"), "Never", "explicit Never respected");
        assert_eq!(
            pol("explicit-ifnp"),
            "IfNotPresent",
            "explicit IfNotPresent respected"
        );
        assert_eq!(
            pol("explicit-always"),
            "Always",
            "explicit Always respected"
        );
        assert_eq!(
            pol("registry-port"),
            "Always",
            "a registry port is not a tag"
        );
        assert_eq!(
            pol("digest"),
            "IfNotPresent",
            "digest refs are immutable -> IfNotPresent"
        );
        // Unknown explicit values fall back to the tag derivation.
        let odd = json!({"metadata": {"name": "x"}, "spec": {"containers": [
            {"name": "c", "image": "img:1.2", "imagePullPolicy": "Sometimes"},
        ]}});
        assert_eq!(pod_containers(&odd)[0].image_pull_policy, "IfNotPresent");
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
        assert_eq!(v.restart_policy, "Always", "absent restartPolicy defaults");
        assert!(pod_view(&json!({})).is_none());
    }

    #[test]
    fn volumes_and_mounts_are_parsed() {
        let p = json!({
            "metadata": {"name": "p", "namespace": "d", "uid": "u"},
            "spec": {
                "volumes": [
                    {"name": "cache", "emptyDir": {}},
                    {"name": "cfg", "configMap": {"name": "cm1"}},
                    {"name": "sec", "secret": {"secretName": "s1"}},
                    {"name": "nope", "hostPath": {"path": "/x"}},
                    {"image": "no-name"},
                ],
                "containers": [{
                    "name": "c", "image": "i",
                    "volumeMounts": [
                        {"name": "cache", "mountPath": "/data"},
                        {"name": "cfg", "mountPath": "/etc/cm", "readOnly": true},
                        {"name": "missing-path"},
                    ],
                }],
            },
        });
        let v = pod_view(&p).unwrap();
        assert_eq!(v.volumes.len(), 3, "unsupported/nameless volumes skipped");
        assert_eq!(
            v.volumes[0],
            VolumeSpec {
                name: "cache".into(),
                kind: VolumeKind::EmptyDir
            }
        );
        assert_eq!(
            v.volumes[1],
            VolumeSpec {
                name: "cfg".into(),
                kind: VolumeKind::ConfigMap { name: "cm1".into() }
            }
        );
        assert_eq!(
            v.volumes[2],
            VolumeSpec {
                name: "sec".into(),
                kind: VolumeKind::Secret { name: "s1".into() }
            }
        );
        let c = &v.containers[0];
        assert_eq!(c.volume_mounts.len(), 2);
        assert_eq!(
            c.volume_mounts[0],
            VolumeMount {
                name: "cache".into(),
                mount_path: "/data".into(),
                read_only: false
            },
            "readOnly defaults to false"
        );
        assert_eq!(
            c.volume_mounts[1],
            VolumeMount {
                name: "cfg".into(),
                mount_path: "/etc/cm".into(),
                read_only: true
            }
        );
        assert!(pod_volumes(&json!({})).is_empty());
        assert!(pod_containers(&json!({})).is_empty());
    }

    #[test]
    fn probes_and_restart_policy_are_parsed() {
        let p = json!({
            "metadata": {"name": "p", "namespace": "d", "uid": "u"},
            "spec": {
                "nodeName": "n",
                "restartPolicy": "OnFailure",
                "containers": [{
                    "name": "c",
                    "image": "i",
                    "startupProbe": {"httpGet": {"path": "/s", "port": 8080}, "failureThreshold": 5},
                    "readinessProbe": {"httpGet": {"path": "/r"}},
                    "livenessProbe": {"httpGet": {"path": "/l"}, "periodSeconds": 3},
                }],
            },
        });
        let v = pod_view(&p).unwrap();
        assert_eq!(v.restart_policy, "OnFailure");
        let c = &v.containers[0];
        let s = c.startup_probe.as_ref().unwrap();
        assert_eq!(s.http_get.path, "/s");
        assert_eq!(s.http_get.port, 8080);
        assert_eq!(s.failure_threshold, 5);
        assert_eq!(c.readiness_probe.as_ref().unwrap().http_get.port, 80);
        assert_eq!(c.liveness_probe.as_ref().unwrap().period_seconds, 3);
        // Unknown restartPolicy values fall back to Always.
        let bad = json!({"metadata": {"name": "x"}, "spec": {"restartPolicy": "Maybe"}});
        assert_eq!(pod_view(&bad).unwrap().restart_policy, "Always");
        // A probe without httpGet is not applicable in S1 -> None.
        let tcp = json!({"metadata": {"name": "x"}, "spec": {"containers": [
            {"name": "c", "image": "i", "livenessProbe": {"tcpSocket": {"port": 1}}}
        ]}});
        assert!(pod_view(&tcp).unwrap().containers[0]
            .liveness_probe
            .is_none());
    }

    #[test]
    fn node_object_shape() {
        let n = node_object("n1", "10.0.0.7", 10250, "2026-08-16T00:00:00Z");
        assert_eq!(n["apiVersion"], "v1");
        assert_eq!(n["kind"], "Node");
        assert_eq!(n["metadata"]["name"], "n1");
        assert_eq!(n["metadata"]["labels"]["kubernetes.io/hostname"], "n1");
        // T4.2 Scope C (Q31): the relay discovery keys.
        assert_eq!(
            n["metadata"]["annotations"]["init-pro.io/kubelet-port"],
            "10250"
        );
        assert_eq!(n["status"]["capacity"]["pods"], "110");
        assert_eq!(n["status"]["conditions"][0]["type"], "Ready");
        assert_eq!(n["status"]["conditions"][0]["status"], "True");
        assert_eq!(
            n["status"]["conditions"][0]["lastHeartbeatTime"],
            "2026-08-16T00:00:00Z"
        );
        assert_eq!(
            n["status"]["addresses"][0],
            json!({"type": "InternalIP", "address": "10.0.0.7"})
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
