//! Integration test for the multicall external-peer stub (T0.1).
//!
//! `external_stub` returns a `process::ExitCode`, whose numeric value is
//! opaque in-process, so we exercise it through the real dispatch path
//! (main -> resolve -> external_stub) by spawning the built binary under a
//! forced argv[0] — the same mechanism a `ln -s init-pro kubectl` symlink uses.

#![cfg(unix)]

use std::os::unix::process::CommandExt;
use std::process::Command;

/// Run the built binary as if invoked through a symlink named `alias`.
fn run_as(alias: &str, args: &[&str]) -> std::process::Output {
    // `CARGO_BIN_EXE_init-pro` is provided by cargo for bin-crate integration
    // tests; `arg0` overrides argv[0] so resolve() sees the alias.
    Command::new(env!("CARGO_BIN_EXE_init-pro"))
        .arg0(alias)
        .args(args)
        .output()
        .expect("spawn init-pro")
}

#[test]
fn external_stub_help_exits_success_with_banner() {
    let out = run_as("kubectl", &["--help"]);
    assert!(
        out.status.success(),
        "help branch must exit success, got {:?}",
        out.status.code()
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("kubectl"), "stdout must name the alias: {stdout}");
    assert!(stdout.contains("stub"), "stdout must mark it as a stub: {stdout}");
    assert!(stdout.contains("Usage:"), "stdout must print usage line: {stdout}");
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
    assert!(stderr.contains("crictl"), "stderr must name the alias: {stderr}");
    assert!(
        stderr.contains("not implemented"),
        "stderr must say not-implemented: {stderr}"
    );
}
