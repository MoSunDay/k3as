//! Pure-helper cases split out of agent_exec_sandbox.rs (file size limit;
//! the server/server_tests precedent). T7.7/Q63 D4 evidence: in-container
//! plan parity, spec decode determinism, attempt-id shape, fail-closed
//! wiring. NO real engine spawn here — that is the e2e's job.
#![cfg(test)]

use std::path::Path;

use serde_json::{json, Value};

use crate::agent_objects as objs;
use crate::agent_sandbox::{select_sandbox, SandboxSelection};
use crate::agent_sandbox_plan::{attempt_id, container_plan, decode_policy, decode_sandbox};

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

/// argv (after argv[0]) mirrors build_spawn_plan flag-for-flag with every
/// host path rewritten to the container layout; envs are the rewritten
/// HOME/XDG/PATH triple.
#[test]
fn container_plan_keeps_spawn_plan_arg_parity() {
    let v = view();
    let (argv, envs) = container_plan(
        &v,
        Some(Path::new("/d/agent-ws/ag-1/prompt.md")),
        "opencoder",
    );
    assert_eq!(argv[0], "/runner/opencoder");
    let legacy = objs::build_spawn_plan(
        &v,
        Path::new("/workspace"),
        Some("/workspace/prompt.md".into()),
        None,
    );
    assert_eq!(
        argv[1..],
        legacy.args[..],
        "same flags as the legacy plan, only paths rewritten"
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
    let (argv2, _) = container_plan(&v, None, "opencoder");
    assert!(!argv2.contains(&"--prompt-file".to_string()));
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
/// config absent selects Invalid — Pending + explicit no-fallback wording,
/// never a silent legacy run (Q63).
#[test]
fn hardened_request_without_node_config_fails_closed() {
    let sandbox = decode_sandbox(&hardened_agent()).unwrap().unwrap();
    match select_sandbox(Some(&sandbox), false) {
        SandboxSelection::Invalid(msg) => {
            assert!(msg.contains("(Q63)"), "msg: {msg}");
            assert!(msg.contains("no fallback"), "msg: {msg}");
        }
        other => panic!("expected Invalid, got {other:?}"),
    }
    // The same request with the containment stack ready selects through.
    assert_eq!(
        select_sandbox(Some(&sandbox), true),
        SandboxSelection::Selected
    );
}
