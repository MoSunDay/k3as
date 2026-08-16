//! Shared scaffolding for the kubelet test binaries (TODO **T4.2**): pod and
//! CRI fixtures for the pure `plan` tests plus an in-memory [`FakeCri`]
//! backend for the end-to-end tests. One shared module keeps every test file
//! under the 400-line cap.
#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

use async_trait::async_trait;
use kubelet::cri_backend::{ContainerView, CriBackend, SandboxView};
use kubelet::{plan, pod_view, Action, PodView, Snapshot};
use runtime::cri_json::{container_labels, pod_labels, ContainerConfig, PodSandboxConfig};

pub const SANDBOX_IMAGE: &str = "registry.k8s.io/pause:3.10";

/// Per-tag log root so parallel test binaries never collide on disk.
pub fn log_root(tag: &str) -> String {
    format!("/tmp/init-pro-kubelet-{tag}/pods")
}

/// One scheduled pod with `images.len()` containers c0..cN.
pub fn view(ns: &str, name: &str, uid: &str, images: &[&str]) -> PodView {
    pod_view(&serde_json::json!({
        "metadata": {"name": name, "namespace": ns, "uid": uid, "resourceVersion": "1"},
        "spec": {"nodeName": "n1", "containers": images.iter()
            .enumerate()
            .map(|(i, img)| serde_json::json!({
                "name": format!("c{i}"), "image": img, "command": [format!("/bin/c{i}")]}))
            .collect::<Vec<_>>()},
    }))
    .unwrap()
}

/// An observed sandbox carrying kubelet pod labels (how `plan` recognizes it).
pub fn sandbox(ns: &str, name: &str, uid: &str, id: &str, state: &str) -> SandboxView {
    SandboxView {
        id: id.into(),
        state: state.into(),
        labels: pod_labels(name, ns, uid),
        name: name.into(),
        namespace: ns.into(),
        uid: uid.into(),
    }
}

/// An observed container labelled as belonging to `pod`.
pub fn container(
    pod: &PodView,
    cname: &str,
    id: &str,
    sb_id: &str,
    state: &str,
    attempt: u32,
) -> ContainerView {
    ContainerView {
        id: id.into(),
        sandbox_id: sb_id.into(),
        state: state.into(),
        name: cname.into(),
        attempt,
        labels: container_labels(&pod.name, &pod.namespace, &pod.uid, cname),
    }
}

pub fn desired_of(views: &[PodView]) -> BTreeMap<String, PodView> {
    views.iter().map(|v| (v.key.clone(), v.clone())).collect()
}

/// Run `plan` for one desired pod (default sandbox image + test log root).
pub fn plan_one(view: &PodView, snap: &Snapshot) -> Vec<Action> {
    plan(
        &desired_of(std::slice::from_ref(view)),
        snap,
        SANDBOX_IMAGE,
        std::path::Path::new("/tmp/init-pro-kubelet-test/pods"),
    )
}

/// First CreateStartContainer's sandbox config (assertion helper).
pub fn pcfg_of(actions: &[Action]) -> &PodSandboxConfig {
    actions
        .iter()
        .find_map(|a| match a {
            Action::CreateStartContainer { pcfg, .. } => Some(pcfg),
            _ => None,
        })
        .expect("a CreateStartContainer action")
}

/// First CreateStartContainer's container config (assertion helper).
pub fn ccfg_of(actions: &[Action]) -> &ContainerConfig {
    actions
        .iter()
        .find_map(|a| match a {
            Action::CreateStartContainer { ccfg, .. } => Some(ccfg),
            _ => None,
        })
        .expect("a CreateStartContainer action")
}

/// The exact sandbox config `plan` builds for default/web (uid u1).
pub fn expect_pcfg(root: &str) -> PodSandboxConfig {
    PodSandboxConfig {
        metadata: runtime::cri_json::SandboxMetadata {
            name: "web".into(),
            namespace: "default".into(),
            uid: "u1".into(),
            attempt: 0,
        },
        labels: pod_labels("web", "default", "u1"),
        annotations: BTreeMap::new(),
        log_directory: format!("{root}/u1"),
    }
}

/// The exact container config `plan` builds for c0 at a given attempt.
pub fn expect_ccfg(pod: &PodView, attempt: u32) -> ContainerConfig {
    ContainerConfig {
        metadata: runtime::cri_json::ContainerMetadata {
            name: "c0".into(),
            attempt,
        },
        image: runtime::cri_json::ImageSpec {
            image: pod.containers[0].image.clone(),
        },
        command: pod.containers[0].command.clone(),
        args: Vec::new(),
        envs: Vec::new(),
        labels: container_labels(&pod.name, &pod.namespace, &pod.uid, "c0"),
        log_path: format!("c0.{attempt}.log"),
    }
}

// ---- FakeCri: in-memory CRI backend with call recording ------------------

#[derive(Default)]
pub struct FakeState {
    pub images: BTreeSet<String>,
    pub sandboxes: Vec<SandboxView>,
    pub containers: Vec<ContainerView>,
    next: u64,
    pub calls: Vec<String>,
}

pub struct FakeCri {
    st: Mutex<FakeState>,
}

impl FakeCri {
    pub fn new() -> Self {
        Self {
            st: Mutex::new(FakeState::default()),
        }
    }
    /// Sync peeks for assertions (the mutex is never held across .await).
    pub fn peek_sandboxes(&self) -> Vec<SandboxView> {
        self.st.lock().unwrap().sandboxes.clone()
    }
    pub fn peek_containers(&self) -> Vec<ContainerView> {
        self.st.lock().unwrap().containers.clone()
    }
    pub fn peek_calls(&self) -> Vec<String> {
        self.st.lock().unwrap().calls.clone()
    }
    /// Test hook: flip a container's state (simulates a crash).
    pub fn flip_container(&self, id: &str, state: &str) {
        let mut st = self.st.lock().unwrap();
        if let Some(c) = st.containers.iter_mut().find(|c| c.id == id) {
            c.state = state.to_string();
        }
    }
}

#[async_trait]
impl CriBackend for FakeCri {
    async fn list_image_tags(&self) -> Result<Vec<String>, String> {
        Ok(self.st.lock().unwrap().images.iter().cloned().collect())
    }
    async fn pull_image(&self, image: &str) -> Result<(), String> {
        let mut st = self.st.lock().unwrap();
        st.images.insert(image.to_string());
        st.calls.push(format!("pull:{image}"));
        Ok(())
    }
    async fn list_sandboxes(&self) -> Result<Vec<SandboxView>, String> {
        Ok(self.peek_sandboxes())
    }
    async fn list_containers(&self) -> Result<Vec<ContainerView>, String> {
        Ok(self.peek_containers())
    }
    async fn run_pod_sandbox(&self, cfg: &PodSandboxConfig) -> Result<String, String> {
        let mut st = self.st.lock().unwrap();
        st.next += 1;
        let id = format!("sb{}", st.next);
        st.calls.push(format!("runp:{}", cfg.metadata.uid));
        st.sandboxes.push(SandboxView {
            id: id.clone(),
            state: "SANDBOX_READY".into(),
            labels: cfg.labels.clone(),
            name: cfg.metadata.name.clone(),
            namespace: cfg.metadata.namespace.clone(),
            uid: cfg.metadata.uid.clone(),
        });
        Ok(id)
    }
    async fn create_container(
        &self,
        pod_sandbox_id: &str,
        ccfg: &ContainerConfig,
        _pcfg: &PodSandboxConfig,
    ) -> Result<String, String> {
        let mut st = self.st.lock().unwrap();
        st.next += 1;
        let id = format!("ct{}", st.next);
        st.calls.push(format!("create:{}", ccfg.metadata.name));
        st.containers.push(ContainerView {
            id: id.clone(),
            sandbox_id: pod_sandbox_id.to_string(),
            state: "CONTAINER_CREATED".into(),
            name: ccfg.metadata.name.clone(),
            attempt: ccfg.metadata.attempt,
            labels: ccfg.labels.clone(),
        });
        Ok(id)
    }
    async fn start_container(&self, id: &str) -> Result<(), String> {
        let mut st = self.st.lock().unwrap();
        st.calls.push(format!("start:{id}"));
        if let Some(c) = st.containers.iter_mut().find(|c| c.id == id) {
            c.state = "CONTAINER_RUNNING".into();
        }
        Ok(())
    }
    async fn stop_container(&self, id: &str) -> Result<(), String> {
        let mut st = self.st.lock().unwrap();
        st.calls.push(format!("stop:{id}"));
        if let Some(c) = st.containers.iter_mut().find(|c| c.id == id) {
            c.state = "CONTAINER_EXITED".into();
        }
        Ok(())
    }
    async fn remove_container(&self, id: &str) -> Result<(), String> {
        let mut st = self.st.lock().unwrap();
        st.calls.push(format!("rm:{id}"));
        st.containers.retain(|c| c.id != id);
        Ok(())
    }
    async fn stop_pod_sandbox(&self, id: &str) -> Result<(), String> {
        let mut st = self.st.lock().unwrap();
        st.calls.push(format!("stopp:{id}"));
        if let Some(s) = st.sandboxes.iter_mut().find(|s| s.id == id) {
            s.state = "SANDBOX_NOTREADY".into();
        }
        Ok(())
    }
    async fn remove_pod_sandbox(&self, id: &str) -> Result<(), String> {
        let mut st = self.st.lock().unwrap();
        st.calls.push(format!("rmp:{id}"));
        st.sandboxes.retain(|s| s.id != id);
        Ok(())
    }
}
