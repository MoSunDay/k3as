//! Pure-helper cases split out of agent_sandbox.rs (file size limit; the
//! server/server_tests precedent). T7.7/Q63 AS2-AS5 evidence: selection
//! fail-closed, spec invariants (tamper matrix), closed-set env, node
//! config from env.
#![cfg(test)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use api::agent_policy::{EffectivePolicy, SandboxLimits};
use engine_proto::ContainerSpec;

use crate::agent_sandbox::{
    prepare_sandbox_dirs, sandbox_spec, sanitize_attempt_id, sanitize_env, select_sandbox,
    validate_hardened, SandboxInputs, SandboxSelection, BROKER_ENV, BROKER_TARGET, RUNNER_TARGET,
    SCRATCH_TARGET, WORKSPACE_TARGET,
};

fn eff() -> EffectivePolicy {
    EffectivePolicy {
        tools: BTreeSet::new(),
        provider_operations: BTreeSet::new(),
        binds: Vec::new(),
        limits: SandboxLimits {
            cpu_millis: 500,
            memory_bytes: 256 * 1024 * 1024,
            pids_max: 64,
        },
        uid: 1000,
        gid: 1000,
    }
}

fn inputs() -> SandboxInputs {
    SandboxInputs {
        attempt_id: "ag-1-uid12345".to_string(),
        rootfs: PathBuf::from("/d/agent-sandbox/ag-1-uid12345/rootfs"),
        eff: eff(),
        workspace_host: PathBuf::from("/d/agent-ws/ag-1"),
        scratch_host: PathBuf::from("/d/agent-sandbox/ag-1-uid12345/scratch"),
        runner_host: Some(PathBuf::from("/d/runner/opencoder")),
        broker_socket: Some(PathBuf::from("/d/run/agent-broker/ag-1-uid12345.sock")),
        argv: vec!["/runner/opencoder".to_string(), "run".to_string()],
        env: sanitize_env(
            &[("HOME".to_string(), "/workspace".to_string())],
            Some(Path::new("/d/run/agent-broker/ag-1-uid12345.sock")),
        ),
        hostname: Some("ag-1".to_string()),
    }
}

#[test]
fn selection_is_fail_closed_not_silent() {
    // No sandbox spec: the legacy Q37 path applies unchanged (no mandate).
    assert_eq!(
        select_sandbox(None, false, false),
        SandboxSelection::NotSelected
    );
    assert_eq!(
        select_sandbox(None, true, false),
        SandboxSelection::NotSelected
    );
    // Hardened + containment stack ready: the sandbox path owns it.
    let spec = api::agent_policy::AgentSandboxSpec {
        mode: "hardened".to_string(),
        uid: None,
        gid: None,
        binds: Vec::new(),
        limits: SandboxLimits {
            cpu_millis: 500,
            memory_bytes: 1,
            pids_max: 8,
        },
    };
    assert_eq!(
        select_sandbox(Some(&spec), true, false),
        SandboxSelection::Selected
    );
    // Hardened + node not ready: Pending + no-fallback wording (Q63).
    match select_sandbox(Some(&spec), false, false) {
        SandboxSelection::Invalid(msg) => {
            assert!(msg.contains("(Q63)"), "msg: {msg}");
            assert!(msg.contains("no fallback"), "msg: {msg}");
            assert!(msg.contains("Pending"), "msg: {msg}");
        }
        other => panic!("expected Invalid, got {other:?}"),
    }
    // The mandate does not soften the hardened failure shape: an
    // unavailable stack is still Invalid (Pending), not RequiredRefused.
    assert!(matches!(
        select_sandbox(Some(&spec), false, true),
        SandboxSelection::Invalid(_)
    ));
}

/// Node-level mandatory hardening (T7.7, Q63): a sandbox-less Agent on a
/// mandating node is RequiredRefused — Pending, never a legacy spawn —
/// regardless of the containment stack state (the refusal precedes it).
#[test]
fn mandatory_hardening_refuses_sandbox_less_agents() {
    for node_ready in [false, true] {
        match select_sandbox(None, node_ready, true) {
            SandboxSelection::RequiredRefused(msg) => {
                assert!(msg.contains("INIT_PRO_AGENT_REQUIRE_SANDBOX"), "msg: {msg}");
                assert!(msg.contains("spec.sandbox"), "msg: {msg}");
                assert!(msg.contains("no legacy spawn"), "msg: {msg}");
            }
            other => panic!("node_ready={node_ready}: expected RequiredRefused, got {other:?}"),
        }
    }
}

#[test]
fn spec_passes_its_own_contract() {
    let spec = sandbox_spec(&inputs());
    assert_eq!(spec.cwd, WORKSPACE_TARGET);
    assert!(spec.new_net);
    assert_eq!(spec.caps, 0);
    assert!(spec.no_new_privs);
    assert_eq!(spec.rootfs, "/d/agent-sandbox/ag-1-uid12345/rootfs");
    // Structural mounts: workspace rw, scratch rw, broker rw. /runner is
    // NOT a bind: nsrun :ro binds are noexec, so the runner is staged
    // INTO the private rootfs (see stage_runner_into_rootfs).
    let targets: Vec<(String, bool)> = spec
        .mounts
        .iter()
        .map(|m| (m.target.clone(), m.readonly))
        .collect();
    assert!(targets.contains(&(WORKSPACE_TARGET.to_string(), false)));
    assert!(targets.contains(&(SCRATCH_TARGET.to_string(), false)));
    assert!(targets.contains(&(BROKER_TARGET.to_string(), false)));
    assert!(
        !targets.iter().any(|(t, _)| t == RUNNER_TARGET),
        "runner must be rootfs content, not a (noexec) ro bind: {targets:?}"
    );
    assert_eq!(
        spec.cpu_max.as_deref(),
        Some("50000 100000"),
        "cpu millis map to quota at the fixed 100000 period (500m = 0.5 CPU)"
    );
    validate_hardened(&spec).unwrap();
}

/// Each tampered invariant is rejected with its OWN actionable message.
#[test]
fn tamper_matrix_is_rejected_with_distinct_messages() {
    let base = sandbox_spec(&inputs());
    let cases: Vec<(ContainerSpec, &str)> = vec![
        {
            let mut s = base.clone();
            s.host_root = true;
            (s, "host_root")
        },
        {
            let mut s = base.clone();
            s.uid = 0;
            (s, "non-root")
        },
        {
            let mut s = base.clone();
            s.caps = 7;
            (s, "empty capability")
        },
        {
            let mut s = base.clone();
            s.pids_max = None;
            (s, "process count must be bounded")
        },
        {
            let mut s = base.clone();
            s.mounts.push(engine_proto::MountSpec {
                source: "/dev".to_string(),
                target: "/dev-host".to_string(),
                readonly: true,
            });
            (s, "host control surface")
        },
    ];
    let mut seen: Vec<String> = Vec::new();
    for (spec, needle) in cases {
        let err = validate_hardened(&spec).unwrap_err();
        assert!(err.contains(needle), "expected {needle:?} in {err:?}");
        assert!(err.contains("(Q63)"), "Q63 remedy missing: {err:?}");
        assert!(
            !seen.contains(&err),
            "messages must be distinct: {err:?} (seen {seen:?})"
        );
        seen.push(err);
    }
}

#[test]
fn readonly_workspace_mount_is_rejected() {
    let mut spec = sandbox_spec(&inputs());
    for m in spec.mounts.iter_mut() {
        if m.target == WORKSPACE_TARGET {
            m.readonly = true;
        }
    }
    let err = validate_hardened(&spec).unwrap_err();
    assert!(err.contains("must not be readonly"), "err: {err}");
}

#[test]
fn env_is_a_closed_order_stable_allowlist() {
    let plan = vec![
        ("HOME".to_string(), "/workspace".to_string()),
        ("XDG_DATA_HOME".to_string(), "/workspace/mem".to_string()),
        ("PATH".to_string(), "/workspace/tools:/runner".to_string()),
    ];
    let broker = Path::new("/d/run/agent-broker/a.sock");
    let env = sanitize_env(&plan, Some(broker));
    assert_eq!(
        env,
        vec![
            "HOME=/workspace".to_string(),
            "XDG_DATA_HOME=/workspace/mem".to_string(),
            "PATH=/workspace/tools:/runner".to_string(),
            format!("{BROKER_ENV}=/d/run/agent-broker/a.sock"),
        ]
    );
    // Without a broker: exactly the plan pairs, nothing more (AS5).
    assert_eq!(sanitize_env(&plan, None).len(), 3);
    // There is no inherited-env parameter at all: nothing else can leak.
}

#[test]
fn attempt_ids_are_sanitized_and_bounded() {
    assert_eq!(sanitize_attempt_id("ag-1"), "ag-1");
    assert_eq!(sanitize_attempt_id("a/b c"), "a-b-c");
    assert_eq!(sanitize_attempt_id(""), "attempt");
    let long = "x".repeat(200);
    assert_eq!(sanitize_attempt_id(&long).len(), 48);
}

#[test]
fn sandbox_dirs_are_private_per_attempt() {
    let dir = std::env::temp_dir().join(format!("ip-sbx-{}", std::process::id()));
    let (rootfs, scratch) = prepare_sandbox_dirs(&dir, "ag-1").unwrap();
    assert_eq!(rootfs, dir.join("agent-sandbox/ag-1/rootfs"));
    assert_eq!(scratch, dir.join("agent-sandbox/ag-1/scratch"));
    assert!(rootfs.is_dir() && scratch.is_dir());
    assert_ne!(rootfs, PathBuf::from("/"));
    // Idempotent.
    prepare_sandbox_dirs(&dir, "ag-1").unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// ONE test fn, both cases sequential: `std::env` is process-global, so
/// parallel test fns racing set_var/remove_var would flake (the env-test
/// doctrine in `crates/kubelet/src/engine/image/pull.rs`).
#[test]
fn node_config_from_env_gates_and_defaults() {
    std::env::remove_var("INIT_PRO_AGENT_SANDBOX");
    std::env::remove_var("INIT_PRO_AGENT_REQUIRE_SANDBOX");
    std::env::remove_var("INIT_PRO_AGENT_BROKER_UPSTREAM");
    std::env::remove_var("INIT_PRO_AGENT_BROKER_OPS");
    std::env::remove_var("INIT_PRO_AGENT_BROKER_CREDENTIAL_HEADER");
    std::env::remove_var("INIT_PRO_AGENT_BROKER_CREDENTIAL");
    // Unset enable flag => None: hardened Agents stay Pending (Q63).
    assert!(crate::agent_sandbox::SandboxNodeConfig::from_env().is_none());
    // Enabled => default baseline, no broker (partial knobs ignored).
    std::env::set_var("INIT_PRO_AGENT_SANDBOX", "1");
    std::env::set_var("INIT_PRO_AGENT_BROKER_CREDENTIAL", "secret-value");
    let cfg = crate::agent_sandbox::SandboxNodeConfig::from_env().unwrap();
    assert_eq!(cfg.baseline.limits_cap.cpu_millis, 2000);
    assert_eq!(cfg.baseline.limits_cap.memory_bytes, 2 * 1024 * 1024 * 1024);
    assert_eq!(cfg.baseline.limits_cap.pids_max, 256);
    assert!(cfg.baseline.bind_allowlist.is_empty());
    assert!(cfg.broker.is_none(), "upstream+ops missing => no broker");
    // All three broker knobs => Some with the default header.
    std::env::set_var("INIT_PRO_AGENT_BROKER_UPSTREAM", "http://127.0.0.1:8080");
    std::env::set_var("INIT_PRO_AGENT_BROKER_OPS", "chat, embed");
    let cfg = crate::agent_sandbox::SandboxNodeConfig::from_env().unwrap();
    let broker = cfg.broker.expect("all knobs present");
    assert_eq!(broker.upstream_url, "http://127.0.0.1:8080");
    assert_eq!(broker.credential_header, "Authorization");
    assert_eq!(broker.credential_value, "secret-value");
    assert_eq!(broker.allowed_operations, vec!["chat", "embed"]);
    // Off value => None again (fail-closed).
    std::env::set_var("INIT_PRO_AGENT_SANDBOX", "0");
    assert!(crate::agent_sandbox::SandboxNodeConfig::from_env().is_none());
    std::env::remove_var("INIT_PRO_AGENT_SANDBOX");
    std::env::remove_var("INIT_PRO_AGENT_BROKER_UPSTREAM");
    std::env::remove_var("INIT_PRO_AGENT_BROKER_OPS");
    std::env::remove_var("INIT_PRO_AGENT_BROKER_CREDENTIAL");
    // T7.7: the mandatory switch alone enables the plane AND flags the
    // mandate (implies enablement; no separate INIT_PRO_AGENT_SANDBOX).
    std::env::set_var("INIT_PRO_AGENT_REQUIRE_SANDBOX", "1");
    let cfg = crate::agent_sandbox::SandboxNodeConfig::from_env().unwrap();
    assert!(cfg.require_hardened, "mandate implied");
    assert!(cfg.broker.is_none(), "broker knobs stay off");
    std::env::set_var("INIT_PRO_AGENT_REQUIRE_SANDBOX", "0");
    assert!(!crate::agent_sandbox::SandboxNodeConfig::from_env().is_some());
    std::env::remove_var("INIT_PRO_AGENT_REQUIRE_SANDBOX");
}
