//! C6 regression cases for the legacy direct-subprocess Agent exec path
//! (T7.1 Phase B, Q37): output capture is tail-bounded, and a timeout or a
//! deletion-cancel kills the runner's WHOLE process group (tools/subagents
//! spawned by opencoder used to survive the direct child's kill).

use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::sync::watch;

use super::*;

fn cfg(exec_timeout: Duration) -> AgentRunnerConfig {
    let mut c =
        AgentRunnerConfig::new("http://127.0.0.1:1".into(), "n".into(), PathBuf::from("/d"));
    c.exec_timeout = exec_timeout;
    c
}

/// A live kill switch (true = keep going): sender + receiver.
fn keep_going() -> (watch::Sender<bool>, watch::Receiver<bool>) {
    watch::channel(true)
}

/// Spawn `sh -c <script>` exactly like the production runner (own process
/// group, kill-on-drop, piped output); returns the child and its pgid.
fn spawn(script: &str) -> (tokio::process::Child, i32) {
    let mut cmd = tokio::process::Command::new("sh");
    cmd.arg("-c")
        .arg(script)
        .process_group(0)
        .kill_on_drop(true)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = cmd.spawn().expect("spawn sh");
    let pgid = group_of(child.id()).expect("pid fits pid_t");
    (child, pgid)
}

/// Process state letter from /proc/<pid>/stat ('Z' = dead, awaiting reap),
/// or None when the pid is gone entirely.
fn proc_state(pid: i32) -> Option<char> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = stat.rsplit_once(')')?.1; // comm may contain spaces/parens
    rest.trim_start().chars().next()
}

/// Poll until `pid` is gone or a zombie; returns the final observation.
async fn settle(pid: i32) -> Option<char> {
    for _ in 0..100 {
        let state = proc_state(pid);
        if state.is_none_or(|s| s == 'Z') {
            return state;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    proc_state(pid)
}

#[test]
fn push_tail_keeps_only_the_last_cap_bytes() {
    let mut kept = Vec::new();
    push_tail(&mut kept, b"hello ", 11);
    push_tail(&mut kept, b"wide ", 11);
    push_tail(&mut kept, b"world", 11);
    assert_eq!(kept, b" wide world"); // last 11 bytes of "hello wide world"
                                      // Below the cap: nothing is dropped.
    let mut small = Vec::new();
    push_tail(&mut small, &[7u8; 5], 64);
    push_tail(&mut small, &[9u8; 3], 64);
    assert_eq!(small, [vec![7u8; 5], vec![9u8; 3]].concat());
    // Exactly at the cap: unchanged; one more byte: the oldest is dropped.
    let mut exact = Vec::new();
    push_tail(&mut exact, &[1u8; 4], 4);
    assert_eq!(exact, [1u8; 4]);
    push_tail(&mut exact, &[2u8; 4], 4);
    assert_eq!(exact, [2u8; 4]);
}

#[tokio::test]
async fn read_tail_caps_a_chatty_stream_to_the_last_64k() {
    let data: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
    // In-memory pipe: a writer task pushes ~200 KiB, the capture keeps 64 KiB.
    let (mut tx, rx) = tokio::io::duplex(4096);
    let payload = data.clone();
    let writer = tokio::spawn(async move {
        tx.write_all(&payload).await.unwrap();
        tx.shutdown().await.unwrap();
    });
    let out = read_tail(Some(rx)).await.unwrap();
    writer.await.unwrap();
    assert_eq!(out.len(), OUTPUT_CAP);
    assert_eq!(out[..], data[data.len() - OUTPUT_CAP..]);
    let none = read_tail::<tokio::io::DuplexStream>(None).await.unwrap();
    assert_eq!(none, Vec::<u8>::new());
}

#[test]
fn group_of_is_the_pid_itself_or_none() {
    assert_eq!(group_of(Some(42)), Some(42));
    assert_eq!(group_of(None), None);
    assert_eq!(group_of(Some(u32::MAX)), None);
}

#[tokio::test]
async fn wait_runner_captures_a_normal_exit() {
    let (child, pgid) = spawn("echo out; echo err >&2; exit 3");
    let (_tx, mut keep) = keep_going();
    match wait_runner(&cfg(Duration::from_secs(20)), child, &mut keep).await {
        Verdict::Done(Ok(out)) => {
            assert_eq!(out.status.code(), Some(3));
            assert_eq!(out.stdout, b"out\n");
            assert_eq!(out.stderr, b"err\n");
        }
        other => panic!("expected Done(Ok(..)), got {other:?}"),
    }
    // A finished runner leaves nothing behind in its group.
    assert!(settle(pgid).await.is_none_or(|s| s == 'Z'));
}

#[tokio::test]
async fn wait_runner_timeout_kills_the_whole_process_group() {
    let marker = std::env::temp_dir().join(format!("init-pro-exec-c6-{}", std::process::id()));
    let _ = std::fs::remove_file(&marker);
    // Grandchild `sleep` outlives the runner; its pid lands in the marker.
    let script = format!("sleep 300 & echo $! > '{}'; sleep 300", marker.display());
    let (child, pgid) = spawn(&script);
    let (_tx, mut keep) = keep_going();
    let verdict = wait_runner(&cfg(Duration::from_millis(400)), child, &mut keep).await;
    assert!(matches!(verdict, Verdict::Timeout), "{verdict:?}");
    // The direct child is gone ...
    assert!(
        settle(pgid).await.is_none_or(|s| s == 'Z'),
        "runner still alive"
    );
    // ... AND so is the tool it spawned (C6: the pre-fix orphan).
    let grandchild: i32 = std::fs::read_to_string(&marker)
        .expect("runner wrote the grandchild pid")
        .trim()
        .parse()
        .expect("numeric pid");
    let _ = std::fs::remove_file(&marker);
    let state = settle(grandchild).await;
    assert!(state.is_none_or(|s| s == 'Z'), "grandchild still {state:?}");
}

#[tokio::test]
async fn wait_runner_cancel_kills_the_whole_process_group() {
    let marker =
        std::env::temp_dir().join(format!("init-pro-exec-c6-cancel-{}", std::process::id()));
    let _ = std::fs::remove_file(&marker);
    let script = format!("sleep 300 & echo $! > '{}'; sleep 300", marker.display());
    let (child, pgid) = spawn(&script);
    let (killer, mut keep) = keep_going();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        let _ = killer.send(false); // object deleted mid-flight
    });
    let verdict = wait_runner(&cfg(Duration::from_secs(20)), child, &mut keep).await;
    assert!(matches!(verdict, Verdict::Cancelled), "{verdict:?}");
    assert!(
        settle(pgid).await.is_none_or(|s| s == 'Z'),
        "runner still alive"
    );
    let grandchild: i32 = std::fs::read_to_string(&marker)
        .expect("runner wrote the grandchild pid")
        .trim()
        .parse()
        .expect("numeric pid");
    let _ = std::fs::remove_file(&marker);
    let state = settle(grandchild).await;
    assert!(state.is_none_or(|s| s == 'Z'), "grandchild still {state:?}");
}
