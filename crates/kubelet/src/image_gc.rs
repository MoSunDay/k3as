//! C3 (bug sweep): the kubelet's periodic image-GC seam.
//!
//! The long-run hazard: pulled images accumulate on the node forever and
//! nothing frees disk once it fills. [`pass`] runs from the sync loop's
//! periodic tick ([`crate::runner`]) once per cycle and hands the backend
//! every image the node's desired pod set still needs; the backend owns
//! the store and the policy:
//!
//! - builtin engine: `nix::sys::statvfs::statvfs` on the image root, then
//!   the pure decision + sweep in [`crate::engine::image::disk`] and
//!   `crate::engine::backend::pressure`;
//! - containerd adapter: the `CriBackend::image_gc` no-op default
//!   (containerd's own image GC owns that store, T4.2 route B).
//!
//! The in-use set has two halves and both are needed: the desired pod set
//! here (what the kubelet is about to run) and the engine's live registry
//! (what is running right now, including containers of pods the kubelet
//! has already forgotten). Neither alone is safe.

use std::collections::BTreeMap;

use crate::cri_backend::CriBackend;
use crate::objects::PodView;

/// Every image the desired pod set needs, sorted and deduped. Deleted pods
/// stay in for the cycle their teardown runs: their containers may still
/// hold unpacked layer dirs as an overlayfs lowerdir.
pub(crate) fn desired_images(desired: &BTreeMap<String, PodView>) -> Vec<String> {
    let mut images: Vec<String> = desired
        .values()
        .flat_map(|pod| pod.containers.iter().map(|c| c.image.clone()))
        .filter(|image| !image.is_empty())
        .collect();
    images.sort();
    images.dedup();
    images
}

/// One GC pass per sync cycle. Cheap by contract: `Healthy` costs a single
/// statvfs, and a pressured root is the only case that walks the store
/// (on the blocking pool -- D). Never fails the reconcile -- the backend
/// logs its own summary line.
pub(crate) async fn pass(cri: &dyn CriBackend, desired: &BTreeMap<String, PodView>) {
    cri.image_gc(&desired_images(desired)).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pod(images: &[&str]) -> PodView {
        PodView {
            key: "default/p".into(),
            namespace: "default".into(),
            name: "p".into(),
            uid: "u".into(),
            containers: images
                .iter()
                .map(|image| crate::objects::ContainerSpec {
                    name: (*image).to_string(),
                    image: (*image).to_string(),
                    command: Vec::new(),
                    args: Vec::new(),
                    startup_probe: None,
                    readiness_probe: None,
                    liveness_probe: None,
                    volume_mounts: Vec::new(),
                    image_pull_policy: String::new(),
                    env: Vec::new(),
                    env_from: Vec::new(),
                    resources: Default::default(),
                })
                .collect(),
            volumes: Vec::new(),
            deleted: false,
            marker: String::new(),
            restart_policy: "Always".into(),
            annotations: Default::default(),
        }
    }

    #[test]
    fn desired_images_dedupes_and_sorts() {
        let mut desired: BTreeMap<String, PodView> = BTreeMap::new();
        desired.insert("default/b".into(), pod(&["z:1", "a:1"]));
        desired.insert("default/a".into(), pod(&["a:1", ""]));
        assert_eq!(
            desired_images(&desired),
            vec!["a:1".to_string(), "z:1".to_string()]
        );
    }

    #[test]
    fn desired_images_keeps_deleted_pods_for_the_teardown_cycle() {
        let mut desired: BTreeMap<String, PodView> = BTreeMap::new();
        let mut gone = pod(&["gone:1"]);
        gone.deleted = true;
        desired.insert("default/gone".into(), gone);
        assert_eq!(desired_images(&desired), vec!["gone:1".to_string()]);
        assert!(desired_images(&BTreeMap::new()).is_empty());
    }

    /// A backend that inherits every default: the containerd adapter's
    /// shape for image GC (it owns a different store).
    struct OtherBackend;

    #[async_trait::async_trait]
    impl CriBackend for OtherBackend {
        async fn list_image_tags(&self) -> Result<Vec<String>, String> {
            Ok(Vec::new())
        }
        async fn pull_image(&self, _image: &str) -> Result<(), String> {
            Ok(())
        }
        async fn list_sandboxes(&self) -> Result<Vec<crate::cri_backend::SandboxView>, String> {
            Ok(Vec::new())
        }
        async fn list_containers(&self) -> Result<Vec<crate::cri_backend::ContainerView>, String> {
            Ok(Vec::new())
        }
        async fn run_pod_sandbox(
            &self,
            _cfg: &runtime::PodSandboxConfig,
        ) -> Result<String, String> {
            Ok(String::new())
        }
        async fn create_container(
            &self,
            _pod_sandbox_id: &str,
            _ccfg: &runtime::ContainerConfig,
            _pcfg: &runtime::PodSandboxConfig,
        ) -> Result<String, String> {
            Ok(String::new())
        }
        async fn start_container(&self, _id: &str) -> Result<(), String> {
            Ok(())
        }
        async fn stop_container(&self, _id: &str) -> Result<(), String> {
            Ok(())
        }
        async fn remove_container(&self, _id: &str) -> Result<(), String> {
            Ok(())
        }
        async fn stop_pod_sandbox(&self, _id: &str) -> Result<(), String> {
            Ok(())
        }
        async fn remove_pod_sandbox(&self, _id: &str) -> Result<(), String> {
            Ok(())
        }
    }

    /// The default `image_gc` is a no-op, so the containerd adapter and
    /// the in-memory test fakes never fail a sync cycle over GC.
    #[tokio::test]
    async fn default_backend_gc_is_a_noop() {
        let other = OtherBackend;
        assert_eq!(
            other.image_gc(&["a:1".to_string()]).await,
            crate::cri_backend::ImageGcStats::default()
        );
        // The wiring itself does not panic on an empty desired set.
        pass(&other, &BTreeMap::new()).await;
    }
}
