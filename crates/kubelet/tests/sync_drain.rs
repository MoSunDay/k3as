//! Shutdown teardown tests for `kubelet::sync::drain_sandboxes` (T4.1
//! follow-up, Sprint 28): every sandbox CRI reports must get stopp -> rmp,
//! already-gone sandboxes are tolerated, and the pass is bounded by the
//! budget. Pure in-memory via the shared [`FakeCri`] — no containerd.

mod support;

use async_trait::async_trait;
use kubelet::cri_backend::CriBackend;
use kubelet::sync::{drain_sandboxes, SANDBOX_DRAIN_BUDGET};
use support::*;
use support::{expect_ccfg, expect_pcfg, view, FakeCri};

#[tokio::test]
async fn drain_stops_and_removes_every_sandbox() {
    let cri = FakeCri::new();
    let pcfg = expect_pcfg(&log_root("drain"));
    cri.run_pod_sandbox(&pcfg).await.unwrap();
    cri.run_pod_sandbox(&pcfg).await.unwrap();
    assert_eq!(cri.peek_sandboxes().len(), 2);

    let n = drain_sandboxes(&cri, SANDBOX_DRAIN_BUDGET).await;
    assert_eq!(n, 2, "both sandboxes attempted");
    assert!(
        cri.peek_sandboxes().is_empty(),
        "every sandbox removed: {:?}",
        cri.peek_sandboxes()
    );
    let calls = cri.peek_calls();
    assert_eq!(
        calls.iter().filter(|c| c.starts_with("stopp:")).count(),
        2,
        "stopp per sandbox: {calls:?}"
    );
    assert_eq!(
        calls.iter().filter(|c| c.starts_with("rmp:")).count(),
        2,
        "rmp per sandbox: {calls:?}"
    );
    // Structural order: each sandbox stops before it is removed.
    for (i, c) in calls.iter().enumerate() {
        if c.starts_with("rmp:") {
            assert!(
                calls[..i].iter().any(|p| p.starts_with("stopp:")),
                "rmp before stopp: {calls:?}"
            );
        }
    }
}

#[tokio::test]
async fn drain_stops_containers_before_their_sandbox() {
    // The regression that bit Sprint 28: StopPodSandbox on a sandbox with a
    // RUNNING container blows crictl's 2s request deadline (DeadlineExceeded)
    // and the rmp then bounces ("is running, please stop it first"). The
    // drain must stop + remove the container (task) first.
    let cri = FakeCri::new();
    let pod = view("default", "web", "u1", &["img:1"]);
    let pcfg = expect_pcfg(&log_root("drain-ct"));
    let sb = cri.run_pod_sandbox(&pcfg).await.unwrap();
    let ccfg = expect_ccfg(&pod, 0);
    let ct = cri.create_container(&sb, &ccfg, &pcfg).await.unwrap();
    cri.start_container(&ct).await.unwrap();

    let n = drain_sandboxes(&cri, SANDBOX_DRAIN_BUDGET).await;
    assert_eq!(n, 1);
    assert!(cri.peek_sandboxes().is_empty());
    let calls = cri.peek_calls();
    let stop_ct = calls.iter().position(|c| c == &format!("stop:{ct}"));
    let rm_ct = calls.iter().position(|c| c == &format!("rm:{ct}"));
    let stopp_sb = calls.iter().position(|c| c.starts_with("stopp:"));
    let rmp_sb = calls.iter().position(|c| c.starts_with("rmp:"));
    assert!(stop_ct.is_some_and(|i| rmp_sb.is_some_and(|j| i < j)), "container stop before sandbox rmp: {calls:?}");
    assert!(rm_ct.is_some_and(|i| stopp_sb.is_some_and(|j| i < j)), "container rm before sandbox stopp: {calls:?}");
    assert!(
        cri.peek_containers().is_empty(),
        "container removed too: {:?}",
        cri.peek_containers()
    );
}

#[tokio::test]
async fn drain_of_empty_cri_is_a_noop() {
    let cri = FakeCri::new();
    let n = drain_sandboxes(&cri, SANDBOX_DRAIN_BUDGET).await;
    assert_eq!(n, 0);
    assert!(
        cri.peek_calls().is_empty(),
        "no CRI calls issued: {calls:?}",
        calls = cri.peek_calls()
    );
}

/// A backend whose `stopp` always fails (the already-gone / wedged case):
/// the drain must still proceed to `rmp` and not give up on the rest.
struct FailingStop(FakeCri);

#[async_trait]
impl CriBackend for FailingStop {
    async fn list_image_tags(&self) -> Result<Vec<String>, String> {
        self.0.list_image_tags().await
    }
    async fn pull_image(&self, image: &str) -> Result<(), String> {
        self.0.pull_image(image).await
    }
    async fn list_sandboxes(&self) -> Result<Vec<kubelet::cri_backend::SandboxView>, String> {
        self.0.list_sandboxes().await
    }
    async fn list_containers(&self) -> Result<Vec<kubelet::cri_backend::ContainerView>, String> {
        self.0.list_containers().await
    }
    async fn run_pod_sandbox(&self, cfg: &runtime::PodSandboxConfig) -> Result<String, String> {
        self.0.run_pod_sandbox(cfg).await
    }
    async fn create_container(
        &self,
        pod_sandbox_id: &str,
        ccfg: &runtime::ContainerConfig,
        pcfg: &runtime::PodSandboxConfig,
    ) -> Result<String, String> {
        self.0.create_container(pod_sandbox_id, ccfg, pcfg).await
    }
    async fn start_container(&self, id: &str) -> Result<(), String> {
        self.0.start_container(id).await
    }
    async fn stop_container(&self, id: &str) -> Result<(), String> {
        self.0.stop_container(id).await
    }
    async fn remove_container(&self, id: &str) -> Result<(), String> {
        self.0.remove_container(id).await
    }
    async fn stop_pod_sandbox(&self, _id: &str) -> Result<(), String> {
        Err("NotFound".into())
    }
    async fn remove_pod_sandbox(&self, id: &str) -> Result<(), String> {
        self.0.remove_pod_sandbox(id).await
    }
}

#[tokio::test]
async fn drain_tolerates_rejected_stops() {
    let inner = FakeCri::new();
    let pcfg = expect_pcfg(&log_root("drain-fail"));
    inner.run_pod_sandbox(&pcfg).await.unwrap();
    let cri = FailingStop(inner);

    let n = drain_sandboxes(&cri, SANDBOX_DRAIN_BUDGET).await;
    assert_eq!(n, 1, "the sandbox was still attempted");
    assert!(
        cri.0.peek_sandboxes().is_empty(),
        "rmp still ran after the rejected stopp"
    );
}

/// A backend whose sandbox list never arrives in time (the wedged-CRI case):
/// the drain must abandon the pass on budget expiry instead of hanging the
/// agent's drain.
struct SlowList(FakeCri);

#[async_trait]
impl CriBackend for SlowList {
    async fn list_image_tags(&self) -> Result<Vec<String>, String> {
        self.0.list_image_tags().await
    }
    async fn pull_image(&self, image: &str) -> Result<(), String> {
        self.0.pull_image(image).await
    }
    async fn list_sandboxes(&self) -> Result<Vec<kubelet::cri_backend::SandboxView>, String> {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        self.0.list_sandboxes().await
    }
    async fn list_containers(&self) -> Result<Vec<kubelet::cri_backend::ContainerView>, String> {
        self.0.list_containers().await
    }
    async fn run_pod_sandbox(&self, cfg: &runtime::PodSandboxConfig) -> Result<String, String> {
        self.0.run_pod_sandbox(cfg).await
    }
    async fn create_container(
        &self,
        pod_sandbox_id: &str,
        ccfg: &runtime::ContainerConfig,
        pcfg: &runtime::PodSandboxConfig,
    ) -> Result<String, String> {
        self.0.create_container(pod_sandbox_id, ccfg, pcfg).await
    }
    async fn start_container(&self, id: &str) -> Result<(), String> {
        self.0.start_container(id).await
    }
    async fn stop_container(&self, id: &str) -> Result<(), String> {
        self.0.stop_container(id).await
    }
    async fn remove_container(&self, id: &str) -> Result<(), String> {
        self.0.remove_container(id).await
    }
    async fn stop_pod_sandbox(&self, id: &str) -> Result<(), String> {
        self.0.stop_pod_sandbox(id).await
    }
    async fn remove_pod_sandbox(&self, id: &str) -> Result<(), String> {
        self.0.remove_pod_sandbox(id).await
    }
}

#[tokio::test]
async fn drain_respects_the_budget_on_a_wedged_cri() {
    let cri = SlowList(FakeCri::new());
    // 50ms budget vs a 200ms list: the pass is abandoned (the caller still
    // proceeds to stop the runtime), nothing is left half-claimed.
    let n = drain_sandboxes(&cri, std::time::Duration::from_millis(50)).await;
    assert_eq!(n, 0, "budget expiry reports nothing attempted");
}
