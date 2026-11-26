//! H1/H6/H4 teardown cases split out of agent_exec_sandbox.rs (file size
//! limit): the bounded teardown on a REAL child process (`sleep` stands in
//! for the engine) and a REAL unix-socket peer (wedged, or closing at
//! once), the drop-path kill, and the repeat-Pending log gate (H2). NO
//! real engine spawn here -- this test binary is not the multicall
//! `init-pro`; that is the e2e's job.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use engine_proto::ContainerSpec;
use serde_json::json;
use tokio::io::AsyncWriteExt;
use tokio::net::UnixListener;
use tokio::sync::Mutex;

use super::*;

/// A fast budget: the production one is 5s/1s/5s (far too slow for a unit
/// test). The SHAPE of the bound is what is under test, not its size.
fn fast_budget() -> TeardownBudget {
    TeardownBudget {
        drain: Duration::from_millis(150),
        bye: Duration::from_millis(150),
        reap: Duration::from_millis(150),
    }
}

/// Isolated per-test data dir (tests share one pid, so the name carries
/// the test too). Short: unix socket paths cap at 108 bytes.
fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ip-sbx-{}-{}", std::process::id(), name));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A live `sleep` standing in for the engine process. `None` on a host
/// without coreutils (`launch_tests.rs` precedent: nothing to assert).
fn live_child() -> Option<Child> {
    Command::new("sleep")
        .arg("60")
        .stdin(Stdio::piped())
        .spawn()
        .ok()
}

fn pid_alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

fn socket_of(dir: &Path, id: &str) -> PathBuf {
    let socket = launch::engine_dir(dir).join(format!("{id}.sock"));
    std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
    socket
}

/// An attempt in the state `run_engine` reaches right after the engine
/// spawn: dirs prepared (with a staged payload), socket present, child
/// live. The socket is a plain FILE here (no engine behind it): a connect
/// to it fails fast, like a socket whose engine is gone.
fn attempt_in(dir: &Path, id: &str, child: Option<Child>) -> Attempt {
    let (rootfs, scratch) = prepare_sandbox_dirs(dir, id).unwrap();
    assert!(rootfs.is_dir() && scratch.is_dir());
    std::fs::write(rootfs.join("payload"), b"runner copy stand-in").unwrap();
    let socket = socket_of(dir, id);
    std::fs::write(&socket, b"").unwrap();
    Attempt {
        id: id.to_string(),
        socket,
        data_dir: dir.to_path_buf(),
        child,
        broker: None,
    }
}

fn assert_cleaned(dir: &Path, id: &str, socket: &Path) {
    assert!(!socket.exists(), "engine socket unlinked");
    assert!(
        !crate::agent_sandbox_plan::attempt_dir(dir, id).exists(),
        "attempt dir removed (H4)"
    );
}

/// H6 (C14): the bug this guards against -- the attempt task is dropped
/// (panic or cancellation) before `teardown` could run. The engine must
/// die AND be reaped (no zombie), and take its socket + dirs along (H4).
#[test]
fn attempt_drop_kills_the_engine_and_removes_the_attempt_dirs() {
    let Some(child) = live_child() else {
        return;
    };
    let dir = temp_dir("drop");
    let attempt = attempt_in(&dir, "ag-drop", Some(child));
    let pid = attempt.child.as_ref().unwrap().id();
    let (socket, id) = (attempt.socket.clone(), attempt.id.clone());
    drop(attempt);
    assert!(!pid_alive(pid), "the stand-in engine is gone and reaped");
    assert_cleaned(&dir, &id, &socket);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// H4 (C14): after `teardown` took the child, the later drop is a pure
/// cleaner (socket + dirs, both idempotent) and never touches a process.
#[test]
fn attempt_drop_without_a_child_only_cleans_up() {
    let dir = temp_dir("drop-nokid");
    let attempt = attempt_in(&dir, "ag-nokid", None);
    let (socket, id) = (attempt.socket.clone(), attempt.id.clone());
    drop(attempt);
    assert_cleaned(&dir, &id, &socket);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// H1 (C5): the `spawn_failed` shape -- no session was ever established.
/// The child is killed, the reap is ours, the dirs go, and the whole
/// teardown is bounded by the budget (never the 5s production grace).
#[tokio::test]
async fn teardown_without_a_session_reaps_and_cleans_within_budget() {
    let Some(child) = live_child() else {
        return;
    };
    let dir = temp_dir("nosession");
    let mut attempt = attempt_in(&dir, "ag-nosession", Some(child));
    let pid = attempt.child.as_ref().unwrap().id();
    let (socket, id) = (attempt.socket.clone(), attempt.id.clone());
    let started = Instant::now();
    let report = teardown(&mut attempt, None, false, fast_budget()).await;
    assert_eq!(
        report,
        TeardownReport {
            drain_timed_out: false,
            reaped: true,
            dirs_removed: true
        }
    );
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "bounded: {:?}",
        started.elapsed()
    );
    assert!(attempt.child.is_none() && attempt.broker.is_none());
    assert!(!pid_alive(pid), "the stand-in engine is gone and reaped");
    assert_cleaned(&dir, &id, &socket);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// H1 (C5): the bug this guards against -- an engine that stops answering
/// mid-drain. The run stream faces a peer that never writes; the teardown
/// must still return within the budget, REPORT the timeout, reap the
/// process and remove the dirs. Before, the `next()` read had no deadline.
#[tokio::test]
async fn teardown_survives_a_wedged_engine() {
    let Some(child) = live_child() else {
        return;
    };
    let dir = temp_dir("wedged");
    let id = "ag-wedged";
    let (rootfs, _scratch) = prepare_sandbox_dirs(&dir, id).unwrap();
    let socket = socket_of(&dir, id);
    // The wedged peer: bound, never accepting, never answering.
    let peer = UnixListener::bind(&socket).unwrap();
    let spec: ContainerSpec =
        serde_json::from_value(json!({"id": id, "rootfs": rootfs, "argv": []})).unwrap();
    let mut stream = EngineClient::connect(&socket)
        .await
        .unwrap()
        .run(spec)
        .await
        .unwrap();
    let mut attempt = Attempt {
        id: id.to_string(),
        socket: socket.clone(),
        data_dir: dir.clone(),
        child: Some(child),
        broker: None,
    };
    let pid = attempt.child.as_ref().unwrap().id();
    let budget = fast_budget();
    let started = Instant::now();
    let report = teardown(&mut attempt, Some(&mut stream), true, budget).await;
    assert!(
        report.drain_timed_out,
        "the wedged read hit the drain budget"
    );
    assert!(report.reaped, "the reap SIGKILL is the backstop");
    assert!(report.dirs_removed, "H4");
    // drain + bye (connect) + bye (reply) + reap + slack -- not the 11s the
    // production budget would take, which proves each wait is budgeted.
    let bound = budget.drain + budget.bye * 2 + budget.reap + Duration::from_secs(3);
    assert!(
        started.elapsed() < bound,
        "teardown outlived its budget: {:?}",
        started.elapsed()
    );
    assert!(!pid_alive(pid), "the stand-in engine died on the reap path");
    assert_cleaned(&dir, id, &socket);
    drop(peer);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// H1 (C5), the fast path: a COOPERATIVE peer (closes at once, like an
/// engine whose session already ended) ends the drain at EOF well before
/// the budget -- the bound is a ceiling, not a fixed wait.
#[tokio::test]
async fn teardown_drains_a_closing_peer_before_the_budget() {
    let Some(child) = live_child() else {
        return;
    };
    let dir = temp_dir("closing");
    let id = "ag-closing";
    let (rootfs, _scratch) = prepare_sandbox_dirs(&dir, id).unwrap();
    let socket = socket_of(&dir, id);
    let listener = UnixListener::bind(&socket).unwrap();
    let closer = tokio::spawn(async move {
        while let Ok((mut conn, _)) = listener.accept().await {
            let _ = conn.shutdown().await;
        }
    });
    let spec: ContainerSpec =
        serde_json::from_value(json!({"id": id, "rootfs": rootfs, "argv": []})).unwrap();
    let mut stream = EngineClient::connect(&socket)
        .await
        .unwrap()
        .run(spec)
        .await
        .unwrap();
    let mut attempt = Attempt {
        id: id.to_string(),
        socket: socket.clone(),
        data_dir: dir.clone(),
        child: Some(child),
        broker: None,
    };
    let started = Instant::now();
    let report = teardown(&mut attempt, Some(&mut stream), true, fast_budget()).await;
    assert!(
        !report.drain_timed_out,
        "EOF ended the drain: not a fixed wait"
    );
    assert!(report.reaped && report.dirs_removed);
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "fast path stays fast: {:?}",
        started.elapsed()
    );
    assert_cleaned(&dir, id, &socket);
    closer.abort();
    std::fs::remove_dir_all(&dir).unwrap();
}

/// H1 (C5): the production bound is the documented one -- drain 5s + two
/// Byes 1s + reap 5s, so a teardown never runs past ~12s + the SIGKILL.
#[test]
fn production_teardown_budget_is_the_documented_bound() {
    assert_eq!(
        TEARDOWN_BUDGET,
        TeardownBudget {
            drain: DRAIN_GRACE,
            bye: BYE_GRACE,
            reap: REAP_GRACE
        }
    );
    let total = TEARDOWN_BUDGET.drain + TEARDOWN_BUDGET.bye * 2 + TEARDOWN_BUDGET.reap;
    assert!(total <= Duration::from_secs(12), "{total:?}");
}

fn view(uid: &str) -> objs::AgentView {
    objs::AgentView {
        key: "default/ag-1".to_string(),
        uid: uid.to_string(),
        name: "ag-1".to_string(),
        namespace: "default".to_string(),
        node_name: None,
        phase: objs::PHASE_PENDING.to_string(),
        prompt: "do it".to_string(),
        model: None,
        agent: None,
    }
}

/// H2 (C14): repeat detection follows the LAST written status body of the
/// same object -- Pending + same reason = repeat; another reason, a
/// terminal phase, no body, no Ready condition, or ANOTHER object (a
/// recreated same-name Agent) = first.
#[tokio::test]
async fn repeat_pending_detection_follows_the_last_written_body() {
    let ag = view("uid-1");
    let written: Written = Arc::new(Mutex::new(BTreeMap::new()));
    assert!(
        !is_repeat_pending(&written, &ag, "SandboxUnavailable").await,
        "no body yet = first"
    );
    let pending = |reason: &str| objs::status_patch(objs::PHASE_PENDING, &[fail_cond(reason, "x")]);
    written
        .lock()
        .await
        .insert(dedupe_id(&ag), pending("SandboxUnavailable"));
    assert!(is_repeat_pending(&written, &ag, "SandboxUnavailable").await);
    assert!(
        !is_repeat_pending(&written, &ag, "SandboxRequired").await,
        "another reason = first"
    );
    assert!(
        !is_repeat_pending(&written, &view("uid-2"), "SandboxUnavailable").await,
        "a recreated object starts over"
    );
    written.lock().await.insert(
        dedupe_id(&ag),
        objs::status_patch(objs::PHASE_FAILED, &[fail_cond("SandboxSpecInvalid", "x")]),
    );
    assert!(
        !is_repeat_pending(&written, &ag, "SandboxSpecInvalid").await,
        "terminal phase = not a Pending repeat"
    );
    written
        .lock()
        .await
        .insert(dedupe_id(&ag), objs::status_patch(objs::PHASE_PENDING, &[]));
    assert!(
        !is_repeat_pending(&written, &ag, "SandboxUnavailable").await,
        "no Ready condition = first"
    );
}

/// H2 (C14): the log gate -- the warn closure runs once (first failure),
/// then only once the reason changes; repeats log at debug instead.
#[tokio::test]
async fn warn_first_pending_warns_once_per_reason() {
    let ag = view("uid-1");
    let written: Written = Arc::new(Mutex::new(BTreeMap::new()));
    let mut warns = 0;
    warn_first_pending(&written, &ag, "SandboxUnavailable", || warns += 1).await;
    assert_eq!(warns, 1, "first failure warns");
    written.lock().await.insert(
        dedupe_id(&ag),
        objs::status_patch(objs::PHASE_PENDING, &[fail_cond("SandboxUnavailable", "x")]),
    );
    warn_first_pending(&written, &ag, "SandboxUnavailable", || warns += 1).await;
    assert_eq!(warns, 1, "a repeat only logs at debug");
    warn_first_pending(&written, &ag, "SandboxRequired", || warns += 1).await;
    assert_eq!(warns, 2, "a new reason is actionable again");
}
