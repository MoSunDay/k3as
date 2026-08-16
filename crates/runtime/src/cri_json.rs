//! CRI JSON data layer (TODO **T4.2**, decision **Q26** route B).
//!
//! Pure serde mirror of the `crictl v1.31.1 -o json` wire shapes — no
//! process spawning here (the subprocess driver lives in [`crate::cri`]).
//! Input configs are Serialize-only and follow the proto field names crictl
//! documents (`log_directory`, `log_path`; its JSON resolver accepts both
//! forms); output listings are Deserialize-only, `#[serde(default)]`-tolerant
//! of missing fields, and read crictl's camelCase output (`podSandboxId`,
//! `createdAt`, `repoTags`).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// k8s CRI label conventions (kubelet-compatible), used by the T4.2
/// reconcilers to map containers/sandboxes back to pods.
pub const LABEL_POD_NAME: &str = "io.kubernetes.pod.name";
pub const LABEL_POD_NAMESPACE: &str = "io.kubernetes.pod.namespace";
pub const LABEL_POD_UID: &str = "io.kubernetes.pod.uid";
pub const LABEL_CONTAINER_NAME: &str = "io.kubernetes.container.name";

/// Standard labels for a pod sandbox (also the prefix of container labels).
pub fn pod_labels(pod_name: &str, pod_namespace: &str, pod_uid: &str) -> BTreeMap<String, String> {
    BTreeMap::from([
        (LABEL_POD_NAME.to_owned(), pod_name.to_owned()),
        (LABEL_POD_NAMESPACE.to_owned(), pod_namespace.to_owned()),
        (LABEL_POD_UID.to_owned(), pod_uid.to_owned()),
    ])
}

/// [`pod_labels`] plus the container name — the full kubelet label set for
/// one container.
pub fn container_labels(
    pod_name: &str,
    pod_namespace: &str,
    pod_uid: &str,
    container_name: &str,
) -> BTreeMap<String, String> {
    let mut labels = pod_labels(pod_name, pod_namespace, pod_uid);
    labels.insert(LABEL_CONTAINER_NAME.to_owned(), container_name.to_owned());
    labels
}

// ---------------------------------------------------------------- inputs --

/// `PodSandboxMetadata` (CRI proto): all four fields round-trip verbatim.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SandboxMetadata {
    pub name: String,
    pub namespace: String,
    pub uid: String,
    pub attempt: u32,
}

/// `ContainerMetadata` (CRI proto).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ContainerMetadata {
    pub name: String,
    pub attempt: u32,
}

/// `ImageSpec` (CRI proto).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ImageSpec {
    pub image: String,
}

/// One `KeyValue` entry of `ContainerConfig.envs`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct KeyValue {
    pub key: String,
    pub value: String,
}

/// Input config for `crictl runp` / sandbox creation. Empty optional-ish
/// collections are omitted so the JSON stays close to the crictl examples.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PodSandboxConfig {
    pub metadata: SandboxMetadata,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub annotations: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub log_directory: String,
}

/// Input config for `crictl create`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ContainerConfig {
    pub metadata: ContainerMetadata,
    pub image: ImageSpec,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub command: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub envs: Vec<KeyValue>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub log_path: String,
}

// --------------------------------------------------------------- outputs --

/// `crictl ps -o json` envelope.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct ListContainers {
    pub containers: Vec<CriContainer>,
}

/// One container row; camelCase wire keys renamed to our snake_case fields.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct CriContainer {
    pub id: String,
    #[serde(rename = "podSandboxId")]
    pub pod_sandbox_id: String,
    pub metadata: ContainerMetadata,
    pub state: String,
    pub labels: BTreeMap<String, String>,
    #[serde(rename = "createdAt")]
    pub created_at: String,
}

/// `crictl pods -o json` envelope (crictl names the list `items`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct ListSandboxes {
    pub items: Vec<CriSandbox>,
}

/// One pod sandbox row.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct CriSandbox {
    pub id: String,
    pub state: String,
    pub metadata: SandboxMetadata,
    pub labels: BTreeMap<String, String>,
    #[serde(rename = "createdAt")]
    pub created_at: String,
}

/// `crictl inspectp <id> -o json` envelope (CRI PodSandboxStatus subset).
/// Sprint 18 / S1: only `status.network` is needed — the pod sandbox IP the
/// kubelet reports as `podIP` (real CNI address, replacing placeholders).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct PodSandboxInspect {
    pub status: PodSandboxInspectStatus,
}

/// `status` subtree of an inspectp envelope.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct PodSandboxInspectStatus {
    pub network: Option<SandboxNetwork>,
}

/// CRI PodSandboxNetworkStatus (`status.network`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct SandboxNetwork {
    pub ip: Option<String>,
}

/// `crictl images -o json` envelope.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct ListImages {
    pub images: Vec<CriImage>,
}

/// One image row (`size`/`uid`/`username` are ignored on purpose).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct CriImage {
    pub id: String,
    #[serde(rename = "repoTags")]
    pub repo_tags: Vec<String>,
    #[serde(rename = "repoDigests")]
    pub repo_digests: Vec<String>,
}

// ----------------------------------------------------------------- parse --

/// First 120 chars of `input` (char-boundary safe) for error messages.
fn snippet(input: &str) -> String {
    input.trim_start().chars().take(120).collect()
}

fn parse_err(what: &str, err: serde_json::Error, input: &str) -> String {
    format!(
        "failed to parse crictl {what} JSON: {err}; input prefix: {:?}",
        snippet(input)
    )
}

/// Parse `crictl ps -a -o json` stdout into container rows.
pub fn parse_containers(stdout: &str) -> Result<Vec<CriContainer>, String> {
    serde_json::from_str::<ListContainers>(stdout)
        .map(|l| l.containers)
        .map_err(|e| parse_err("containers", e, stdout))
}

/// Parse `crictl pods -o json` stdout into sandbox rows.
pub fn parse_sandboxes(stdout: &str) -> Result<Vec<CriSandbox>, String> {
    serde_json::from_str::<ListSandboxes>(stdout)
        .map(|l| l.items)
        .map_err(|e| parse_err("sandboxes", e, stdout))
}

/// Parse `crictl inspectp <id> -o json` stdout (Sprint 18 / S1).
pub fn parse_inspect_pod_sandbox(stdout: &str) -> Result<PodSandboxInspect, String> {
    serde_json::from_str::<PodSandboxInspect>(stdout).map_err(|e| parse_err("inspectp", e, stdout))
}

/// Parse `crictl images -o json` stdout into image rows.
pub fn parse_images(stdout: &str) -> Result<Vec<CriImage>, String> {
    serde_json::from_str::<ListImages>(stdout)
        .map(|l| l.images)
        .map_err(|e| parse_err("images", e, stdout))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Real crictl v1.31.1 output shapes (camelCase keys).
    const CONTAINERS_ONE: &str = r#"{"containers":[{"id":"abc","podSandboxId":"sb1","metadata":{"name":"c","attempt":0},"state":"CONTAINER_RUNNING","labels":{"io.kubernetes.pod.name":"p"},"createdAt":"2026-08-16T10:00:00Z"}]}"#;
    const CONTAINERS_EMPTY: &str = r#"{"containers":[]}"#;
    const SANDBOXES_ONE: &str = r#"{"items":[{"id":"sb1","state":"SANDBOX_READY","metadata":{"name":"p","namespace":"default","uid":"u1","attempt":0},"labels":{},"createdAt":"2026-08-16T10:00:00Z"}]}"#;
    const IMAGES_ONE: &str = r#"{"images":[{"id":"sha256:0f0","repoTags":["init-pro.local/pause:0.1"],"repoDigests":[]}]}"#;
    // Real `crictl inspectp` capture (containerd 1.7.20): only
    // `status.network` matters here, everything else must be tolerated.
    const INSPECTP_ONE: &str = r#"{"status":{"id":"d2e7","metadata":{"name":"demo","namespace":"default","uid":"u","attempt":0},"state":"SANDBOX_READY","createdAt":"2026-08-16T15:00:00Z","network":{"ip":"10.42.0.10","additionalIps":[]},"labels":{},"annotations":{},"runtimeHandler":"","linux":{}},"info":{}}"#;

    #[test]
    fn parse_containers_running_fixture() {
        let cs = parse_containers(CONTAINERS_ONE).unwrap();
        assert_eq!(cs.len(), 1);
        let c = &cs[0];
        assert_eq!(c.id, "abc");
        assert_eq!(c.pod_sandbox_id, "sb1");
        assert_eq!(c.metadata.name, "c");
        assert_eq!(c.metadata.attempt, 0);
        assert_eq!(c.state, "CONTAINER_RUNNING");
        assert_eq!(c.labels.get(LABEL_POD_NAME).map(String::as_str), Some("p"));
        assert_eq!(c.created_at, "2026-08-16T10:00:00Z");
    }

    #[test]
    fn parse_containers_empty_list() {
        assert!(parse_containers(CONTAINERS_EMPTY).unwrap().is_empty());
    }

    #[test]
    fn parse_containers_tolerates_missing_fields() {
        let cs = parse_containers(r#"{"containers":[{"id":"x"}]}"#).unwrap();
        assert_eq!(cs.len(), 1);
        assert_eq!(cs[0].id, "x");
        assert!(cs[0].pod_sandbox_id.is_empty());
        assert!(cs[0].state.is_empty());
        assert!(cs[0].labels.is_empty());
    }

    #[test]
    fn parse_sandboxes_fixture() {
        let ss = parse_sandboxes(SANDBOXES_ONE).unwrap();
        assert_eq!(ss.len(), 1);
        let s = &ss[0];
        assert_eq!(s.id, "sb1");
        assert_eq!(s.state, "SANDBOX_READY");
        assert_eq!(s.metadata.name, "p");
        assert_eq!(s.metadata.namespace, "default");
        assert_eq!(s.metadata.uid, "u1");
        assert_eq!(s.created_at, "2026-08-16T10:00:00Z");
    }

    #[test]
    fn parse_sandboxes_empty_envelope() {
        assert!(parse_sandboxes(r#"{"items":[]}"#).unwrap().is_empty());
    }

    #[test]
    fn parse_inspect_pod_sandbox_fixture_ip() {
        let p = parse_inspect_pod_sandbox(INSPECTP_ONE).unwrap();
        assert_eq!(
            p.status.network.as_ref().and_then(|n| n.ip.as_deref()),
            Some("10.42.0.10")
        );
    }

    #[test]
    fn parse_inspect_pod_sandbox_missing_network() {
        let p = parse_inspect_pod_sandbox(r#"{"status":{}}"#).unwrap();
        assert!(p.status.network.is_none(), "no network -> no ip");
    }

    #[test]
    fn parse_inspect_pod_sandbox_garbage_errors() {
        let err = parse_inspect_pod_sandbox("not json").unwrap_err();
        assert!(err.contains("inspectp"), "{err}");
    }

    #[test]
    fn parse_images_fixture() {
        let imgs = parse_images(IMAGES_ONE).unwrap();
        assert_eq!(imgs.len(), 1);
        assert_eq!(imgs[0].id, "sha256:0f0");
        assert_eq!(imgs[0].repo_tags, vec!["init-pro.local/pause:0.1"]);
        assert!(imgs[0].repo_digests.is_empty());
    }

    #[test]
    fn parse_garbage_reports_input_prefix() {
        let garbage = "this is not json at all — logrus noise";
        let err = parse_containers(garbage).unwrap_err();
        assert!(err.contains("containers"), "kind named: {err}");
        assert!(err.contains("this is not json"), "prefix kept: {err}");
        assert!(parse_sandboxes("{oops").is_err());
        assert!(parse_images("[1,2,3").is_err());
    }

    #[test]
    fn pod_sandbox_config_serializes_snake_case_and_skips_empties() {
        let cfg = PodSandboxConfig {
            metadata: SandboxMetadata {
                name: "p".into(),
                namespace: "default".into(),
                uid: "u1".into(),
                attempt: 0,
            },
            labels: BTreeMap::new(),
            annotations: BTreeMap::new(),
            log_directory: String::new(),
        };
        let json = serde_json::to_string(&cfg).unwrap();
        assert!(!json.contains("labels"), "empty maps omitted: {json}");
        assert!(!json.contains("annotations"));
        assert!(!json.contains("log_directory"), "empty dir omitted");
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["metadata"]["uid"], "u1");
    }

    #[test]
    fn container_label_round_trip() {
        let cfg = ContainerConfig {
            metadata: ContainerMetadata {
                name: "web".into(),
                attempt: 1,
            },
            image: ImageSpec {
                image: "init-pro.local/pause:0.1".into(),
            },
            command: vec!["/pause".into()],
            args: vec![],
            envs: vec![KeyValue {
                key: "K".into(),
                value: "V".into(),
            }],
            labels: container_labels("p", "ns", "u1", "web"),
            log_path: "web.0.log".into(),
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["args"], serde_json::Value::Null, "empty vec omitted");
        let labels: BTreeMap<String, String> = serde_json::from_value(v["labels"].clone()).unwrap();
        assert_eq!(labels, container_labels("p", "ns", "u1", "web"));
        assert_eq!(v["log_path"], "web.0.log");
    }

    #[test]
    fn label_helpers_follow_k8s_conventions() {
        let p = pod_labels("p", "ns", "u1");
        assert_eq!(p.len(), 3);
        assert_eq!(p[LABEL_POD_NAME], "p");
        assert_eq!(p[LABEL_POD_NAMESPACE], "ns");
        assert_eq!(p[LABEL_POD_UID], "u1");
        let c = container_labels("p", "ns", "u1", "web");
        assert_eq!(c.len(), 4);
        assert_eq!(c[LABEL_CONTAINER_NAME], "web");
        assert!(c.contains_key(LABEL_POD_UID), "pod labels are the prefix");
    }
}
