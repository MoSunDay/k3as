//! Integration test for the multicall external-peer stub (T0.1).
//!
//! `external_stub` returns a `process::ExitCode`, whose numeric value is
//! opaque in-process, so we exercise it through the real dispatch path
//! (main -> resolve -> external_stub) by spawning the built binary under a
//! forced argv[0] — the same mechanism a `ln -s init-pro ctr` symlink uses.
//! (`kubectl` no longer routes to the stub since T3.1b; it gets its own
//! assertion below.)
//!
//! Isolation (S47 review F4): the host `/var/lib/init-pro` may hold staged
//! peers (`agent/containerd/…`) and a `run/engine/mode` marker left behind
//! by prior e2e runs; those would make the child exec a real peer (or
//! refuse as a built-in-engine node) instead of answering as the stub. Each
//! `run_as` invocation therefore pins `INIT_PRO_DATA_DIR` — the documented
//! resolution seam the multicall itself honors — at a fresh empty temp dir,
//! so the child resolves no staged peer and the stub branch is exercised
//! deterministically.

#![cfg(unix)]

use std::os::unix::process::CommandExt;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Per-invocation sequence so every spawned child gets its own unique
/// isolated data dir (see `run_as`).
static CALL_SEQ: AtomicUsize = AtomicUsize::new(0);

/// Run the built binary as if invoked through a symlink named `alias`.
///
/// `INIT_PRO_DATA_DIR` points at a fresh empty temp dir so the child finds
/// no staged peers and no engine-mode file, keeping the stub branch
/// deterministic regardless of host state (S47 review F4).
fn run_as(alias: &str, args: &[&str]) -> std::process::Output {
    // `CARGO_BIN_EXE_init-pro` is provided by cargo for bin-crate integration
    // tests; `arg0` overrides argv[0] so resolve() sees the alias.
    let seq = CALL_SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("init-pro-stub-{}-{}", std::process::id(), seq));
    std::fs::create_dir_all(&dir).expect("create isolated peer data dir");
    // The dir is deliberately leaked for the child's lifetime: the child
    // resolves peers under it while running, and the OS reclaims /tmp
    // entries. Multicall semantics are unchanged — only exec staged peers —
    // the test just removes every staged peer the child can see.
    Command::new(env!("CARGO_BIN_EXE_init-pro"))
        .arg0(alias)
        .env("INIT_PRO_DATA_DIR", &dir)
        .args(args)
        .output()
        .expect("spawn init-pro")
}

#[test]
fn external_stub_help_exits_success_with_banner() {
    // ctr is still a Phase 1 stub (kubectl moved to a real surface, T3.1b).
    let out = run_as("ctr", &["--help"]);
    assert!(
        out.status.success(),
        "help branch must exit success, got {:?}",
        out.status.code()
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("ctr"),
        "stdout must name the alias: {stdout}"
    );
    assert!(
        stdout.contains("stub"),
        "stdout must mark it as a stub: {stdout}"
    );
    assert!(
        stdout.contains("Usage:"),
        "stdout must print usage line: {stdout}"
    );
}

#[test]
fn kubectl_help_exits_success_with_real_usage() {
    // Since T3.1b, kubectl routes to the in-repo implementation (Q21), not
    // the stub: `--help` must still exit 0 and print real usage.
    let out = run_as("kubectl", &["--help"]);
    assert!(
        out.status.success(),
        "kubectl --help must exit success, got {:?}",
        out.status.code()
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("rollout status"),
        "stdout must document the v1 surface: {stdout}"
    );
    assert!(
        stdout.contains("Usage:"),
        "stdout must print usage line: {stdout}"
    );
    assert!(
        !stdout.contains("stub"),
        "kubectl is no longer a stub: {stdout}"
    );
}

#[test]
fn external_stub_without_help_exits_2_with_stderr() {
    let out = run_as("crictl", &[]);
    assert_eq!(
        out.status.code(),
        Some(2),
        "no-help branch must exit 2, got {:?}",
        out.status.code()
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("crictl"),
        "stderr must name the alias: {stderr}"
    );
    assert!(
        stderr.contains("not implemented"),
        "stderr must say not-implemented: {stderr}"
    );
}
