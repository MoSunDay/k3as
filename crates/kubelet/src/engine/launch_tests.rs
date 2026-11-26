//! H5/H6 (C14) launch-helper cases split out of `launch.rs` (file size
//! limit; the `gpu_tests.rs` precedent): the engine log cap, the parent
//! tie flag on the engine command line, and the bounded reaps -- plus the
//! D offloaded reap / spawn-settle cases.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use super::*;

fn args_of(cmd: &Command) -> Vec<String> {
    cmd.get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect()
}

/// H5 (C14): under the cap the log is appended to (post-mortem of several
/// short attempts survives); at or over the cap the NEXT launch truncates,
/// so a deterministic id keeps only the latest attempt's bytes.
#[test]
fn engine_log_is_capped_to_the_latest_attempt() {
    let dir = std::env::temp_dir().join(format!("ip-launch-log-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let log = dir.join("a.sock.engine.log");
    writeln!(open_engine_log(&log).unwrap(), "first").unwrap();
    writeln!(open_engine_log(&log).unwrap(), "second").unwrap();
    assert_eq!(std::fs::read_to_string(&log).unwrap(), "first\nsecond\n");
    let big = vec![b'x'; (ENGINE_LOG_CAP + 1) as usize];
    std::fs::write(&log, &big).unwrap();
    assert!(truncate_log(Some(big.len() as u64)));
    writeln!(open_engine_log(&log).unwrap(), "latest").unwrap();
    assert_eq!(std::fs::read_to_string(&log).unwrap(), "latest\n");
    // Exactly at the cap truncates; under it appends; a missing file is
    // created (never truncated away).
    assert!(truncate_log(Some(ENGINE_LOG_CAP)));
    assert!(!truncate_log(Some(ENGINE_LOG_CAP - 1)));
    assert!(!truncate_log(None));
    std::fs::remove_dir_all(&dir).unwrap();
}

/// H6 (C14): every engine command line carries the parent-tie flag, for
/// normal and pause engines alike.
#[test]
fn engine_command_carries_the_parent_tie_flag() {
    let cmd = engine_command(Path::new("/tmp/x/run/engine/a.sock"), false, false).unwrap();
    assert_eq!(
        args_of(&cmd),
        vec![
            "engine",
            "serve",
            "--socket",
            "/tmp/x/run/engine/a.sock",
            "--exit-on-stdin-eof",
        ]
    );
    let pause = engine_command(Path::new("/tmp/x/run/engine/p.sock"), true, true).unwrap();
    assert_eq!(
        args_of(&pause),
        vec![
            "engine",
            "serve",
            "--socket",
            "/tmp/x/run/engine/p.sock",
            "--pause",
            "--host-net",
            "--exit-on-stdin-eof",
        ]
    );
    let stderr = engine_stderr_path(Path::new("/tmp/x/run/engine/a.sock"));
    assert_eq!(stderr, Path::new("/tmp/x/run/engine/a.sock.engine.log"));
}

/// H6 (C14) drop-path reap on a real child (`sleep` stands in for the
/// engine): SIGKILL + a bounded wait leaves no zombie behind.
#[test]
fn kill_and_reap_reaps_a_real_child() {
    let Ok(mut child) = Command::new("sleep")
        .arg("60")
        .stdin(Stdio::piped())
        .spawn()
    else {
        return; // no coreutils on this host: nothing to assert
    };
    assert!(
        child.stdin.is_some(),
        "the piped stdin lives inside the Child handle (parent tie)"
    );
    let started = Instant::now();
    assert!(kill_and_reap(&mut child, Duration::from_secs(5)));
    assert!(started.elapsed() < Duration::from_secs(5), "bounded reap");
    assert!(child.try_wait().unwrap().is_some(), "reaped, not a zombie");
}

/// `reap` still returns promptly for a child that exits on its own.
#[test]
fn reap_returns_once_the_child_exits() {
    let Ok(mut child) = Command::new("true").spawn() else {
        return;
    };
    reap(&mut child, Duration::from_secs(5)).unwrap();
    assert!(child.try_wait().unwrap().is_some());
}

/// D: the offloaded reap (the async callers' path) is the same bounded
/// reap -- a `sleep` stand-in is gone and reaped, never parked on a
/// tokio worker.
#[tokio::test]
async fn reap_offload_reaps_a_real_child() {
    let Ok(child) = Command::new("sleep")
        .arg("60")
        .stdin(Stdio::piped())
        .spawn()
    else {
        return;
    };
    let pid = child.id();
    reap_offload(child, Duration::from_millis(100))
        .await
        .unwrap();
    assert_gone(pid).await;
}

/// D: an engine that never binds fails `settle` within the socket budget
/// AND is killed + reaped (no zombie until the kubelet exits); an engine
/// that bound hands the very same child back to its owner.
#[tokio::test]
async fn settle_kills_an_engine_that_never_binds() {
    let dir = std::env::temp_dir().join(format!("ip-launch-settle-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let socket = dir.join("never.sock");
    let Ok(child) = Command::new("sleep")
        .arg("60")
        .stdin(Stdio::piped())
        .spawn()
    else {
        let _ = std::fs::remove_dir_all(&dir);
        return;
    };
    let pid = child.id();
    let started = Instant::now();
    let err = settle_within(child, &socket, "pause engine", Duration::from_millis(200))
        .await
        .expect_err("the socket never appears");
    assert!(
        err.contains("pause engine never came up: engine never bound"),
        "{err}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "bounded by the socket budget"
    );
    assert_gone(pid).await;
    // Bound: the child comes back untouched for its owner.
    let bound = dir.join("bound.sock");
    let child = Command::new("sleep")
        .arg("60")
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    let pid = child.id();
    std::fs::write(&bound, b"").unwrap();
    let mut child = settle_within(child, &bound, "engine", Duration::from_millis(200))
        .await
        .unwrap();
    assert_eq!(child.id(), pid, "the very same child");
    assert!(child.try_wait().unwrap().is_none(), "still running");
    reap_offload(child, Duration::ZERO).await.unwrap();
    assert_gone(pid).await;
    std::fs::remove_dir_all(&dir).unwrap();
}

/// `/proc/<pid>` disappears within a generous budget: the SIGKILL was
/// delivered AND the exit waited on (a zombie would keep the entry).
async fn assert_gone(pid: u32) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Path::new(&format!("/proc/{pid}")).exists() {
        assert!(Instant::now() < deadline, "pid {pid} left behind (zombie?)");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
