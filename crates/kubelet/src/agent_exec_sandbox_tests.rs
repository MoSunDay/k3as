//! Pure-helper cases split out of agent_exec_sandbox.rs (file size limit;
//! the server/server_tests precedent). T7.7/Q63 D4 evidence: in-container
//! plan parity, spec decode determinism, attempt-id shape, fail-closed
//! wiring. NO real engine spawn here — that is the e2e's job.
#![cfg(test)]

use std::path::Path;

use serde_json::{json, Value};

use crate::agent_objects as objs;
use crate::agent_sandbox::{select_sandbox, SandboxSelection};
use crate::agent_sandbox_plan::{
    attempt_id, audit_denial_count, container_plan, decode_policy, decode_sandbox,
    write_policy_file,
};

fn view() -> objs::AgentView {
    objs::AgentView {
        key: "default/ag-1".to_string(),
        uid: "0f4ca93e-77c8-4d61-a1b2-000000000001".to_string(),
        name: "ag-1".to_string(),
        namespace: "default".to_string(),
        node_name: None,
        phase: "Pending".to_string(),
        prompt: "do the thing".to_string(),
        model: Some("openai/gpt".to_string()),
        agent: Some("act".to_string()),
    }
}

fn hardened_agent() -> Value {
    json!({
        "apiVersion": "init-pro.io/v1", "kind": "Agent",
        "metadata": {"name": "ag-1", "namespace": "default"},
        "spec": {
            "prompt": "do it",
            "sandbox": {"mode": "hardened",
                        "limits": {"cpuMillis": 500, "memoryBytes": 268435456, "pidsMax": 64}}
        }
    })
}

/// T7.7 D7 (Q63): argv targets the HARDENED agent-runner — the gated
/// in-process tool registry — with the fail-closed policy file, the
/// advisory audit JSONL and the attempt id (session id) wired in; envs
/// stay the closed HOME/XDG/PATH triple. NO `run` subcommand and NO
/// parity with the legacy opencode spawn plan (that CLI is not argv[0]).
#[test]
fn container_plan_targets_the_hardened_agent_runner() {
    let v = view();
    let attempt = "ag-1-00000001";
    let (argv, envs) = container_plan(
        &v,
        Some(Path::new("/d/agent-ws/ag-1/prompt.md")),
        "agent-runner",
        attempt,
    );
    assert_eq!(
        argv,
        vec![
            "/runner/agent-runner".to_string(),
            "--workdir".to_string(),
            "/workspace".to_string(),
            "--policy".to_string(),
            api::agent_policy::SANDBOX_POLICY_FILE.to_string(),
            "--audit".to_string(),
            api::agent_policy::SANDBOX_AUDIT_FILE.to_string(),
            "--attempt".to_string(),
            attempt.to_string(),
            "--model".to_string(),
            "openai/gpt".to_string(),
            "--agent".to_string(),
            "act".to_string(),
            "--prompt-file".to_string(),
            "/workspace/prompt.md".to_string(),
            "do the thing".to_string(),
        ]
    );
    assert_eq!(
        envs,
        vec![
            ("HOME".to_string(), "/workspace".to_string()),
            ("XDG_DATA_HOME".to_string(), "/workspace/mem".to_string()),
            ("PATH".to_string(), "/workspace/tools:/runner".to_string()),
        ]
    );
    // Without a prompt file the flag pair is simply absent.
    let (argv2, _) = container_plan(&v, None, "agent-runner", attempt);
    assert!(!argv2.contains(&"--prompt-file".to_string()));
    // A custom hardened-runner basename is honored at argv[0] (the env
    // override names any executable file).
    let (argv3, _) = container_plan(&v, None, "my-runner", attempt);
    assert_eq!(argv3[0], "/runner/my-runner");
}

/// The policy file is the camelCase wire form (Q10) at
/// `<ws>/.init-pro/policy.json`, 0644, and round-trips as the exact
/// `EffectivePolicy` the hardened agent-runner fail-closes on.
#[test]
fn policy_file_is_the_camel_case_wire_form() {
    use std::collections::BTreeSet;
    use std::os::unix::fs::PermissionsExt;
    let ws = std::env::temp_dir().join(format!("ip-plc-{}", std::process::id()));
    let eff = api::agent_policy::EffectivePolicy {
        tools: BTreeSet::from([api::agent_policy::ToolClass::Ordinary]),
        provider_operations: BTreeSet::new(),
        binds: Vec::new(),
        limits: api::agent_policy::SandboxLimits {
            cpu_millis: 500,
            memory_bytes: 268435456,
            pids_max: 64,
        },
        uid: 1000,
        gid: 1000,
    };
    let path = write_policy_file(&ws, &eff).unwrap();
    assert_eq!(path, ws.join(".init-pro/policy.json"));
    let raw: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(raw["tools"], serde_json::json!(["ordinary"]));
    assert_eq!(raw["limits"]["cpuMillis"], 500);
    assert_eq!(raw["uid"], 1000);
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(
        mode, 0o644,
        "readable by the sandbox uid, node-owned writes"
    );
    // Round-trip: the runner's Deserialize sees the identical policy.
    assert_eq!(
        serde_json::from_slice::<api::agent_policy::EffectivePolicy>(
            &std::fs::read(&path).unwrap()
        )
        .unwrap(),
        eff
    );
    let _ = std::fs::remove_dir_all(&ws);
}

/// The advisory audit count matches THIS attempt's ActionDenied lines only
/// (foreign attempts, non-denial events and garbage never count); an
/// absent file reports None (nothing to say).
#[test]
fn audit_denial_count_matches_only_this_attempt() {
    let ws = std::env::temp_dir().join(format!("ip-aud-{}", std::process::id()));
    assert_eq!(audit_denial_count(&ws, "ag-1-00000001"), None);
    std::fs::create_dir_all(ws.join(".init-pro")).unwrap();
    let audit = api::agent_policy::denial_audit(
        "ag-1",
        "ag-1-00000001",
        api::agent_policy::ToolClass::Custom,
        "bash",
        "not in the effective tool set",
    );
    std::fs::write(
        ws.join(".init-pro/audit.jsonl"),
        format!(
            "{}\n{}\n{}\n{}\n",
            audit,
            serde_json::json!({"type": "ActionDenied", "attempt": "other-99999999"}),
            serde_json::json!({"type": "SomethingElse", "attempt": "ag-1-00000001"}),
            "not json at all",
        ),
    )
    .unwrap();
    assert_eq!(audit_denial_count(&ws, "ag-1-00000001"), Some(1));
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn decode_is_deterministic_for_present_malformed_specs() {
    // Absent and explicit-null => None => the legacy Q37 path continues.
    assert_eq!(decode_sandbox(&json!({"spec": {"prompt": "x"}})), None);
    let mut null_sandbox = hardened_agent();
    null_sandbox["spec"]["sandbox"] = Value::Null;
    assert_eq!(decode_sandbox(&null_sandbox), None);
    // Present + well-formed => Some(Ok).
    let ok = decode_sandbox(&hardened_agent()).unwrap().unwrap();
    assert_eq!(ok.mode, "hardened");
    // Present + malformed (limits missing) => Some(Err), deterministic.
    let mut bad = hardened_agent();
    bad["spec"]["sandbox"] = json!({"mode": "hardened"});
    let err = decode_sandbox(&bad).unwrap().unwrap_err();
    assert!(err.starts_with("spec.sandbox:"), "err: {err}");
    // Same contract for the optional policy overlay.
    assert_eq!(decode_policy(&hardened_agent()), None);
    let mut bad_policy = hardened_agent();
    bad_policy["spec"]["policy"] = json!({"allowTools": ["nope"]});
    assert!(decode_policy(&bad_policy).unwrap().is_err());
}

/// Deterministic per-object id: sanitized name + uid tail; a recreated
/// object (new uid) gets a new id, a re-run keeps it.
#[test]
fn attempt_id_is_name_plus_uid_tail() {
    let mut v = view();
    let id = attempt_id(&v);
    assert!(id.starts_with("ag-1-"), "id: {id}");
    assert_eq!(id.len(), "ag-1-".len() + 8);
    // Same object re-run: same id.
    assert_eq!(attempt_id(&v), id);
    // Recreated object: new uid tail => new id.
    v.uid = "0f4ca93e-77c8-4d61-a1b2-000000000002".to_string();
    assert_ne!(attempt_id(&v), id);
    // Hostile name chars are sanitized before they reach a path.
    v.name = "a/b c".to_string();
    assert!(
        attempt_id(&v).starts_with("a-b-c-"),
        "id: {}",
        attempt_id(&v)
    );
}

/// Wiring shape: a hardened spec decoded from raw Agent JSON with the node
/// containment stack absent selects Invalid — Pending + explicit
/// no-fallback wording, never a silent legacy run (Q63). The node-level
/// mandatory switch refuses sandbox-less Agents instead (RequiredRefused).
#[test]
fn hardened_request_without_node_config_fails_closed() {
    let sandbox = decode_sandbox(&hardened_agent()).unwrap().unwrap();
    match select_sandbox(Some(&sandbox), false, false) {
        SandboxSelection::Invalid(msg) => {
            assert!(msg.contains("(Q63)"), "msg: {msg}");
            assert!(msg.contains("no fallback"), "msg: {msg}");
            assert!(msg.contains("Pending"), "msg: {msg}");
            assert!(
                msg.contains("INIT_PRO_AGENT_HARDENED_RUNNER_BIN"),
                "msg must name the hardened agent-runner staging: {msg}"
            );
        }
        other => panic!("expected Invalid, got {other:?}"),
    }
    // The same request with the containment stack ready selects through.
    assert_eq!(
        select_sandbox(Some(&sandbox), true, false),
        SandboxSelection::Selected
    );
    // Node mandates hardening: a sandbox-less Agent is refused BEFORE any
    // legacy spawn could run (T7.7, Q63).
    match select_sandbox(None, true, true) {
        SandboxSelection::RequiredRefused(msg) => {
            assert!(msg.contains("INIT_PRO_AGENT_REQUIRE_SANDBOX"), "msg: {msg}");
            assert!(msg.contains("no legacy spawn"), "msg: {msg}");
        }
        other => panic!("expected RequiredRefused, got {other:?}"),
    }
}
