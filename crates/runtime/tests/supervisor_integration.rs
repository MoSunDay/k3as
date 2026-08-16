//! T4.1 integration: supervise a REAL vendored containerd end-to-end.
//!
//! Covers the S2 acceptance: the supervisor brings the staged daemon up
//! (socket health), a `kill -9` of the child is survived (rebirth with a new
//! pid + socket healthy again), crictl round-trips `version`/`ps` through
//! the CRI socket, and shutdown drains the child.
//!
//! SKIP semantics (documented in Q25): when `vendor/bin/containerd` is absent
//! (Auto acquire mode + empty cache, e.g. a fresh clone without
//! `INIT_PRO_VENDOR=1`), these tests print `SKIP` and pass — the golden G24
//! gate still hard-fails in populated environments.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use infra::Shutdown;
use runtime::{AgentRuntimePaths, ContainerdConfigVars, DEFAULT_SANDBOX_IMAGE};
use tokio::sync::watch;

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
    let d = std::env::temp_dir().join(format!("rt-sup-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn kill9(pid: u32) {
    Command::new("kill")
        .arg("-9")
        .arg(pid.to_string())
        .status()
        .expect("kill -9 helper runs");
}

async fn wait_pid(rx: &mut watch::Receiver<Option<u32>>) -> Option<u32> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(pid) = *rx.borrow() {
            return Some(pid);
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        let _ = tokio::time::timeout(Duration::from_secs(5), rx.changed()).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn supervisor_restarts_after_kill9_and_drains_on_shutdown() {
    let Some(vendor) = repo_vendor_bin() else {
        eprintln!("SKIP: vendor/bin/containerd absent (INIT_PRO_VENDOR=1 cargo build)");
        return;
    };
    let dd = temp_data_dir("rebirth");
    let paths = AgentRuntimePaths::for_data_dir(&dd);
    let outcome = runtime::stage_containerd_tree(&vendor, &paths.runtime_dir).unwrap();
    assert!(
        !outcome.copied.is_empty() || !outcome.skipped.is_empty(),
        "containerd must be staged"
    );

    std::fs::create_dir_all(dd.join("agent/etc/containerd")).unwrap();
    let vars = ContainerdConfigVars::for_data_dir(&dd, DEFAULT_SANDBOX_IMAGE);
    std::fs::write(&paths.config_path, runtime::render(&vars)).unwrap();
    runtime::stage::write_cni_conf(&vars.cni_conf_dir).unwrap();
    let _ = std::fs::remove_file(&paths.socket);

    let mut spec = runtime::SupervisorSpec::new(
        paths.runtime_dir.join("containerd"),
        &paths.config_path,
        &paths.socket,
    );
    spec.path_prefix = Some(paths.runtime_dir.clone());

    let shutdown = Shutdown::new();
    let (pid_tx, mut pid_rx) = watch::channel::<Option<u32>>(None);
    let task = tokio::spawn(runtime::supervise(spec, shutdown.clone(), pid_tx));

    // 1. Boot: pid appears and the socket accepts connections.
    let pid1 = wait_pid(&mut pid_rx).await.expect("child pid reported");
    assert!(
        runtime::wait_socket(&paths.socket, Duration::from_secs(20)).await,
        "socket healthy after supervised boot"
    );

    // 2. kill -9 the child; the supervisor must respawn it.
    kill9(pid1);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let mut pid2 = None;
    while tokio::time::Instant::now() < deadline {
        if let Some(p) = *pid_rx.borrow() {
            if p != pid1 {
                pid2 = Some(p);
                break;
            }
        }
        let _ = tokio::time::timeout(Duration::from_millis(200), pid_rx.changed()).await;
    }
    let pid2 = pid2.unwrap_or_else(|| panic!("child {pid1} not reborn after kill -9"));
    assert!(
        runtime::wait_socket(&paths.socket, Duration::from_secs(20)).await,
        "socket healthy again after rebirth"
    );

    // 3. Drain: shutdown kills the child and the task completes.
    shutdown.trigger();
    let stats = tokio::time::timeout(Duration::from_secs(30), task)
        .await
        .expect("supervisor task drains within 30s")
        .expect("task not panicked");
    assert!(stats.ever_healthy, "session saw a healthy socket");
    assert!(stats.restarts >= 1, "kill -9 counted as a restart");
    assert_eq!(*pid_rx.borrow(), None, "pid channel closed on drain");
    // The child is really gone: signal 0 fails.
    let alive = Command::new("kill")
        .arg("-0")
        .arg(pid2.to_string())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    assert!(!alive, "drained child must not survive");

    let _ = std::fs::remove_dir_all(&dd);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn staged_crictl_round_trips_over_cri_socket() {
    let Some(vendor) = repo_vendor_bin() else {
        eprintln!("SKIP: vendor/bin/containerd absent (INIT_PRO_VENDOR=1 cargo build)");
        return;
    };
    if !vendor.join("crictl").is_file() {
        eprintln!("SKIP: vendor/bin/crictl not fetched yet (S4 pin)");
        return;
    }
    let dd = temp_data_dir("crictl");
    let paths = AgentRuntimePaths::for_data_dir(&dd);
    runtime::stage_containerd_tree(&vendor, &paths.runtime_dir).unwrap();
    assert!(paths.runtime_dir.join("crictl").is_file(), "crictl staged");

    std::fs::create_dir_all(dd.join("agent/etc/containerd")).unwrap();
    let vars = ContainerdConfigVars::for_data_dir(&dd, DEFAULT_SANDBOX_IMAGE);
    std::fs::write(&paths.config_path, runtime::render(&vars)).unwrap();
    runtime::stage::write_cni_conf(&vars.cni_conf_dir).unwrap();
    let _ = std::fs::remove_file(&paths.socket);

    let mut spec = runtime::SupervisorSpec::new(
        paths.runtime_dir.join("containerd"),
        &paths.config_path,
        &paths.socket,
    );
    spec.path_prefix = Some(paths.runtime_dir.clone());
    let shutdown = Shutdown::new();
    let (pid_tx, _rx) = watch::channel::<Option<u32>>(None);
    let task = tokio::spawn(runtime::supervise(spec, shutdown.clone(), pid_tx));
    assert!(
        runtime::wait_socket(&paths.socket, Duration::from_secs(20)).await,
        "containerd healthy before crictl calls"
    );

    let crictl = paths.runtime_dir.join("crictl");
    let endpoint = format!("unix://{}", paths.socket.display());
    // `crictl version` exercises the CRI RuntimeService.Version RPC — the
    // semantic round-trip golden G24 asserts (route-B evidence for Q26).
    let version = Command::new(&crictl)
        .arg("--runtime-endpoint")
        .arg(&endpoint)
        .arg("version")
        .output()
        .expect("crictl exec");
    let stdout = String::from_utf8_lossy(&version.stdout);
    assert!(
        version.status.success() && stdout.contains("containerd"),
        "crictl version round-trip failed: {stdout}{}",
        String::from_utf8_lossy(&version.stderr)
    );

    // `crictl ps` on an empty daemon exits 0 (empty listing).
    let ps = Command::new(&crictl)
        .arg("--runtime-endpoint")
        .arg(&endpoint)
        .arg("ps")
        .output()
        .expect("crictl ps exec");
    assert!(
        ps.status.success(),
        "crictl ps failed: {}",
        String::from_utf8_lossy(&ps.stderr)
    );

    shutdown.trigger();
    let _ = tokio::time::timeout(Duration::from_secs(30), task).await;
    let _ = std::fs::remove_dir_all(&dd);
}
