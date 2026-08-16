//! CRI backend seam (TODO **T4.2**, decisions **Q26**/**Q27**).
//!
//! [`CriBackend`] is the trait the reconcile core ([`crate::sync`]) programs
//! against; [`CriCtlBackend`] adapts the vendored-crictl subprocess driver
//! ([`runtime::CriCtl`], Q26 route B). Q27 keeps this seam so a native CRI
//! gRPC implementation can replace the adapter later without touching the
//! sync loop. Views flatten the crictl JSON listings into the fields the
//! planner needs; errors are plain strings (errors as values).

use std::collections::BTreeMap;

use runtime::cri_json::{
    CriContainer, CriSandbox, LABEL_CONTAINER_NAME, LABEL_POD_NAME, LABEL_POD_NAMESPACE,
    LABEL_POD_UID,
};
use runtime::{ContainerConfig, CriCtl, PodSandboxConfig};

/// One observed pod sandbox, flattened from `crictl pods -o json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxView {
    pub id: String,
    pub state: String,
    pub labels: BTreeMap<String, String>,
    pub name: String,
    pub namespace: String,
    pub uid: String,
    /// CNI IP of the sandbox (`crictl inspectp status.network.ip`);
    /// None until the sandbox is READY / Sprint 18 S1.
    pub ip: Option<String>,
}

/// One observed container, flattened from `crictl ps -a -o json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerView {
    pub id: String,
    pub sandbox_id: String,
    pub state: String,
    pub name: String,
    pub attempt: u32,
    pub labels: BTreeMap<String, String>,
}

/// The CRI surface the kubelet equivalent drives. Implemented by
/// [`CriCtlBackend`] in production and by in-memory fakes in tests.
#[async_trait::async_trait]
pub trait CriBackend: Send + Sync {
    /// Repo tags of all present images (`crictl images`).
    async fn list_image_tags(&self) -> Result<Vec<String>, String>;
    /// Pull one image by tag (`crictl pull`).
    async fn pull_image(&self, image: &str) -> Result<(), String>;
    /// All pod sandboxes, every state (`crictl pods`).
    async fn list_sandboxes(&self) -> Result<Vec<SandboxView>, String>;
    /// All containers, every state (`crictl ps -a`).
    async fn list_containers(&self) -> Result<Vec<ContainerView>, String>;
    /// `crictl runp` -> the new sandbox id.
    async fn run_pod_sandbox(&self, cfg: &PodSandboxConfig) -> Result<String, String>;
    /// `crictl create` -> the new container id.
    async fn create_container(
        &self,
        pod_sandbox_id: &str,
        ccfg: &ContainerConfig,
        pcfg: &PodSandboxConfig,
    ) -> Result<String, String>;
    async fn start_container(&self, id: &str) -> Result<(), String>;
    async fn stop_container(&self, id: &str) -> Result<(), String>;
    async fn remove_container(&self, id: &str) -> Result<(), String>;
    async fn stop_pod_sandbox(&self, id: &str) -> Result<(), String>;
    async fn remove_pod_sandbox(&self, id: &str) -> Result<(), String>;
}

/// Adapter: [`runtime::CriCtl`] subprocess driver -> [`CriBackend`].
#[derive(Debug, Clone)]
pub struct CriCtlBackend {
    inner: CriCtl,
}

impl CriCtlBackend {
    pub fn new(inner: CriCtl) -> Self {
        Self { inner }
    }
}

/// Sandbox identity: prefer CRI metadata fields, fall back to the
/// `io.kubernetes.*` labels crictl also carries.
fn sandbox_view(s: &CriSandbox) -> SandboxView {
    let from_label = |k: &str| s.labels.get(k).cloned().unwrap_or_default();
    SandboxView {
        id: s.id.clone(),
        state: s.state.clone(),
        name: if s.metadata.name.is_empty() {
            from_label(LABEL_POD_NAME)
        } else {
            s.metadata.name.clone()
        },
        namespace: if s.metadata.namespace.is_empty() {
            from_label(LABEL_POD_NAMESPACE)
        } else {
            s.metadata.namespace.clone()
        },
        uid: if s.metadata.uid.is_empty() {
            from_label(LABEL_POD_UID)
        } else {
            s.metadata.uid.clone()
        },
        labels: s.labels.clone(),
        ip: None,
    }
}

fn container_view(c: &CriContainer) -> ContainerView {
    ContainerView {
        id: c.id.clone(),
        sandbox_id: c.pod_sandbox_id.clone(),
        state: c.state.clone(),
        name: if c.metadata.name.is_empty() {
            c.labels
                .get(LABEL_CONTAINER_NAME)
                .cloned()
                .unwrap_or_default()
        } else {
            c.metadata.name.clone()
        },
        attempt: c.metadata.attempt,
        labels: c.labels.clone(),
    }
}

#[async_trait::async_trait]
impl CriBackend for CriCtlBackend {
    async fn list_image_tags(&self) -> Result<Vec<String>, String> {
        let images = self.inner.list_images().await.map_err(|e| format!("{e}"))?;
        Ok(images
            .into_iter()
            .flat_map(|i| i.repo_tags)
            .collect::<Vec<_>>())
    }

    async fn pull_image(&self, image: &str) -> Result<(), String> {
        self.inner
            .pull_image(image)
            .await
            .map_err(|e| format!("{e}"))
    }

    async fn list_sandboxes(&self) -> Result<Vec<SandboxView>, String> {
        let sandboxes = self
            .inner
            .list_pod_sandboxes()
            .await
            .map_err(|e| format!("{e}"))?;
        let mut views: Vec<SandboxView> = sandboxes.into_iter().map(|s| sandbox_view(&s)).collect();
        // The listing carries no IP; ask inspectp per READY sandbox (Sprint 18
        // / S1) so pod status reports the real CNI address.
        for view in views.iter_mut() {
            if view.state == "SANDBOX_READY" {
                if let Ok(inspect) = self.inner.inspect_pod_sandbox(&view.id).await {
                    view.ip = inspect
                        .status
                        .network
                        .as_ref()
                        .and_then(|n| n.ip.clone())
                        .filter(|ip| !ip.is_empty());
                }
            }
        }
        Ok(views)
    }

    async fn list_containers(&self) -> Result<Vec<ContainerView>, String> {
        let containers = self
            .inner
            .list_containers()
            .await
            .map_err(|e| format!("{e}"))?;
        Ok(containers.into_iter().map(|c| container_view(&c)).collect())
    }

    async fn run_pod_sandbox(&self, cfg: &PodSandboxConfig) -> Result<String, String> {
        self.inner
            .run_pod_sandbox(cfg)
            .await
            .map_err(|e| format!("{e}"))
    }

    async fn create_container(
        &self,
        pod_sandbox_id: &str,
        ccfg: &ContainerConfig,
        pcfg: &PodSandboxConfig,
    ) -> Result<String, String> {
        self.inner
            .create_container(pod_sandbox_id, ccfg, pcfg)
            .await
            .map_err(|e| format!("{e}"))
    }

    async fn start_container(&self, id: &str) -> Result<(), String> {
        self.inner
            .start_container(id)
            .await
            .map_err(|e| format!("{e}"))
    }

    async fn stop_container(&self, id: &str) -> Result<(), String> {
        // 10s grace matches `crictl stop --timeout 10` (Q26 defaults).
        self.inner
            .stop_container(id, 10)
            .await
            .map_err(|e| format!("{e}"))
    }

    async fn remove_container(&self, id: &str) -> Result<(), String> {
        self.inner
            .remove_container(id)
            .await
            .map_err(|e| format!("{e}"))
    }

    async fn stop_pod_sandbox(&self, id: &str) -> Result<(), String> {
        self.inner
            .stop_pod_sandbox(id)
            .await
            .map_err(|e| format!("{e}"))
    }

    async fn remove_pod_sandbox(&self, id: &str) -> Result<(), String> {
        self.inner
            .remove_pod_sandbox(id)
            .await
            .map_err(|e| format!("{e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sandbox_view_prefers_metadata_then_labels() {
        let s = runtime::cri_json::CriSandbox {
            id: "sb1".into(),
            state: "SANDBOX_READY".into(),
            metadata: runtime::cri_json::SandboxMetadata {
                name: "p".into(),
                namespace: "ns".into(),
                uid: "u1".into(),
                attempt: 0,
            },
            labels: BTreeMap::from([(LABEL_POD_NAME.to_string(), "other".to_string())]),
            created_at: String::new(),
        };
        let v = sandbox_view(&s);
        assert_eq!(
            (v.name.as_str(), v.namespace.as_str(), v.uid.as_str()),
            ("p", "ns", "u1")
        );
        // Empty metadata falls back to labels (garbage -> empty strings).
        let s2 = runtime::cri_json::CriSandbox {
            id: "sb2".into(),
            labels: BTreeMap::from([
                (LABEL_POD_NAME.to_string(), "ln".into()),
                (LABEL_POD_NAMESPACE.to_string(), "lns".into()),
                (LABEL_POD_UID.to_string(), "lu".into()),
            ]),
            ..Default::default()
        };
        let v2 = sandbox_view(&s2);
        assert_eq!(
            (v2.name.as_str(), v2.namespace.as_str(), v2.uid.as_str()),
            ("ln", "lns", "lu")
        );
        let v3 = sandbox_view(&runtime::cri_json::CriSandbox::default());
        assert_eq!((v3.name.as_str(), v3.namespace.as_str()), ("", ""));
    }

    #[test]
    fn container_view_prefers_metadata_name() {
        let c = runtime::cri_json::CriContainer {
            id: "c1".into(),
            pod_sandbox_id: "sb1".into(),
            metadata: runtime::cri_json::ContainerMetadata {
                name: "web".into(),
                attempt: 3,
            },
            state: "CONTAINER_RUNNING".into(),
            labels: BTreeMap::from([(LABEL_CONTAINER_NAME.to_string(), "x".into())]),
            created_at: String::new(),
        };
        let v = container_view(&c);
        assert_eq!(v.name, "web");
        assert_eq!(v.attempt, 3);
        let c2 = runtime::cri_json::CriContainer {
            labels: BTreeMap::from([(LABEL_CONTAINER_NAME.to_string(), "lbl".into())]),
            ..Default::default()
        };
        assert_eq!(container_view(&c2).name, "lbl");
    }
}
