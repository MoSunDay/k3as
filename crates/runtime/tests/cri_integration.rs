//! T4.2 integration: drive a REAL vendored containerd through the CRI
//! driver ([`runtime::CriCtl`], Q26 route B).
//!
//! Boots the agent runtime end-to-end ([`runtime::start_agent_runtime`])
//! against a temp data dir, waits for the CRI socket, then round-trips
//! `version` / `images` / `ps` / `pods` through the staged crictl. No
//! image pulls: registry egress is unreliable in CI.
//!
//! SKIP semantics (same as supervisor_integration.rs, documented in Q25):
//! when `vendor/bin/containerd` is absent (Auto acquire + empty cache),
//! the test prints `SKIP` and passes; golden gates hard-fail in populated
//! environments.

use std::path::{Path, PathBuf};
use std::time::Duration;

use infra::Shutdown;
use runtime::{AgentRuntimePaths, CriCtl};

fn repo_vendor_bin() -> Option<PathBuf> {
    let env = std::env::var_os("INIT_PRO_VENDOR_BIN").map(PathBuf::from);
    if let Some(v) = env {
        if v.join("containerd").is_file() {
            return Some(v);
        }
    }
    // tests run from crates/runtime; the repo root is two levels up.
    let cand = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vendor/bin");
    cand.join("containerd").is_file().then_some(cand)
}

fn temp_data_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("rt-cri-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cri_driver_round_trips_against_live_containerd() {
    let Some(vendor) = repo_vendor_bin() else {
        eprintln!("SKIP: vendor/bin/containerd absent (INIT_PRO_VENDOR=1 cargo build)");
        return;
    };
    let dd = temp_data_dir("roundtrip");
    let paths = AgentRuntimePaths::for_data_dir(&dd);
    // Pre-stage so start_agent_runtime takes its "reuse staged tree" branch
    // (test binaries can't rely on the exe-relative vendor discovery).
    runtime::stage_containerd_tree(&vendor, &paths.runtime_dir).unwrap();

    let shutdown = Shutdown::new();
    let task = runtime::start_agent_runtime(&dd, shutdown.clone())
        .expect("agent runtime boots from the staged tree");
    assert!(
        runtime::wait_socket(&paths.socket, Duration::from_secs(30)).await,
        "CRI socket healthy before crictl calls"
    );

    let cri = CriCtl::for_paths(&paths).expect("staged crictl present after staging");
    // Version exercises RuntimeService.Version — the Q26 route-B evidence.
    let version = cri.version().await.expect("crictl version succeeds");
    assert!(
        version.contains("containerd"),
        "version names the runtime: {version}"
    );
    // Empty-daemon listings: the calls succeed and parse to empty sets.
    let images = cri.list_images().await.expect("crictl images succeeds");
    let containers = cri.list_containers().await.expect("crictl ps succeeds");
    let sandboxes = cri
        .list_pod_sandboxes()
        .await
        .expect("crictl pods succeeds");
    assert!(images.is_empty(), "fresh daemon has no images: {images:?}");
    assert!(
        containers.is_empty(),
        "fresh daemon has no containers: {containers:?}"
    );
    assert!(
        sandboxes.is_empty(),
        "fresh daemon has no sandboxes: {sandboxes:?}"
    );

    shutdown.trigger();
    let stats = tokio::time::timeout(Duration::from_secs(30), task)
        .await
        .expect("supervisor drains within 30s")
        .expect("supervisor task did not panic");
    assert!(stats.ever_healthy, "session saw a healthy socket");
    let _ = std::fs::remove_dir_all(&dd);
}
