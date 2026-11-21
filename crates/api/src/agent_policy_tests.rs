//! T7.7/Q63 unit evidence (AS1 scope) for `agent_policy`: default-deny gate
//! behavior, narrowing-only `effective` evaluation, bind allowlist matching,
//! mandatory bounded limits, non-root identity, verbatim subagent
//! inheritance, the secret-free denial audit, serde camelCase wire shapes,
//! and the `validate::admit` dispatch for `init-pro.io/Agent`.

use std::collections::BTreeSet;

use serde_json::{json, Value};

use crate::agent::AgentSpec;
use crate::agent_policy::*; // T7.7/Q63 surface under test
use crate::validate::admit;

fn bind(src: &str, dst: &str, ro: bool) -> BindRule {
    BindRule {
        source: src.to_string(),
        target: dst.to_string(),
        readonly: ro,
    }
}

fn limits(cpu: u64, mem: u64, pids: u64) -> SandboxLimits {
    SandboxLimits {
        cpu_millis: cpu,
        memory_bytes: mem,
        pids_max: pids,
    }
}

fn not_allowed(src: &str, dst: &str) -> PolicyViolation {
    PolicyViolation::BindNotAllowlisted {
        source: src.to_string(),
        target: dst.to_string(),
    }
}

/// Operator baseline fixture: two tool classes, two provider ops, one
/// read-only bind slot, one read-write slot, a 1000m/1GiB/256-pids cap.
fn baseline() -> PolicyBaseline {
    PolicyBaseline {
        tools: BTreeSet::from([ToolClass::Ordinary, ToolClass::Mcp]),
        provider_operations: BTreeSet::from(["files.read".to_string(), "files.write".to_string()]),
        bind_allowlist: vec![bind("/data", "/data", true), bind("/work", "/work", false)],
        limits_cap: limits(1000, 1 << 30, 256),
    }
}

/// Valid hardened sandbox request comfortably under the cap. Built via serde
/// so the fixture doubles as wire-shape evidence.
fn hardened() -> AgentSandboxSpec {
    serde_json::from_value(json!({"mode": "hardened", "limits": limits_json()})).unwrap()
}

fn limits_json() -> Value {
    json!({"cpuMillis": 500, "memoryBytes": 1048576, "pidsMax": 64})
}

fn agent_body(spec: Value) -> Value {
    json!({
        "apiVersion": "init-pro.io/v1", "kind": "Agent",
        "metadata": {"name": "a1", "namespace": "default"}, "spec": spec
    })
}

// --- gate / default deny -----------------------------------------------------

#[test]
fn default_deny_baseline_denies_every_tool_class() {
    let deny = PolicyBaseline::deny_all();
    let eff = EffectivePolicy {
        tools: deny.tools.clone(),
        provider_operations: BTreeSet::new(),
        binds: Vec::new(),
        limits: limits(1, 1, 1),
        uid: SANDBOX_UID,
        gid: SANDBOX_GID,
    };
    for class in ToolClass::ALL {
        let GateDecision::Deny(reason) = gate(&eff, class, "bash") else {
            panic!("{} allowed under deny-all", class.as_str())
        };
        assert!(
            reason.contains(class.as_str()) && reason.contains("bash"),
            "{reason}"
        );
    }
    // Under the zero-cap baseline no hardened sandbox can be effective at
    // all (every positive limit exceeds the cap) — deny by construction.
    assert_eq!(
        effective(&deny, None, &hardened()),
        Err(PolicyViolation::LimitsExceedCap("cpuMillis"))
    );
}

#[test]
fn tool_class_wire_names_roundtrip() {
    for class in ToolClass::ALL {
        assert_eq!(ToolClass::parse(class.as_str()), Some(class));
    }
    assert_eq!(ToolClass::parse("nuclear"), None);
    let violation = PolicyViolation::UnknownToolClass("nuclear".to_string());
    assert!(violation.reason().contains("nuclear"));
}

// --- effective: narrowing-only intersection ----------------------------------

#[test]
fn effective_intersects_tools_and_rejects_widening() {
    let b = baseline();
    let narrow = AgentPolicySpec {
        allow_tools: Some(vec![ToolClass::Ordinary]),
        ..Default::default()
    };
    let eff = effective(&b, Some(&narrow), &hardened()).unwrap();
    assert_eq!(eff.tools, BTreeSet::from([ToolClass::Ordinary]));
    let GateDecision::Allow = gate(&eff, ToolClass::Ordinary, "read") else {
        panic!()
    };
    let GateDecision::Deny(_) = gate(&eff, ToolClass::Mcp, "mcp.call") else {
        panic!()
    };
    // Absent overlay inherits the baseline verbatim.
    assert_eq!(effective(&b, None, &hardened()).unwrap().tools, b.tools);
    // A class outside the baseline is a deterministic widening rejection
    // with an auditable reason.
    let wide = AgentPolicySpec {
        allow_tools: Some(vec![ToolClass::Ssh]),
        ..Default::default()
    };
    let err = effective(&b, Some(&wide), &hardened()).unwrap_err();
    assert_eq!(err, PolicyViolation::WidenedTool(ToolClass::Ssh));
    assert_eq!(
        err.reason(),
        "spec.policy.allowTools: Forbidden: class \"ssh\" is not in the operator baseline \
         (Q63: policies may only narrow)"
    );
}

#[test]
fn effective_narrows_and_rejects_widened_provider_ops() {
    let b = baseline();
    let narrow = AgentPolicySpec {
        provider_operations: Some(vec!["files.read".to_string()]),
        ..Default::default()
    };
    let eff = effective(&b, Some(&narrow), &hardened()).unwrap();
    assert_eq!(
        eff.provider_operations,
        BTreeSet::from(["files.read".to_string()])
    );
    let wide = AgentPolicySpec {
        provider_operations: Some(vec!["net.fetch".to_string()]),
        ..Default::default()
    };
    assert_eq!(
        effective(&b, Some(&wide), &hardened()),
        Err(PolicyViolation::WidenedProviderOp("net.fetch".to_string()))
    );
}

#[test]
fn bind_allowlist_ro_slot_satisfies_only_ro_requests() {
    let b = baseline();
    let ro = AgentPolicySpec {
        binds: vec![bind("/data", "/data", true)],
        ..Default::default()
    };
    assert!(effective(&b, Some(&ro), &hardened()).is_ok());
    // A read-only allowlist slot never satisfies a read-write request.
    let mut rw = ro.clone();
    rw.binds[0].readonly = false;
    assert_eq!(
        effective(&b, Some(&rw), &hardened()),
        Err(not_allowed("/data", "/data"))
    );
    // A read-write slot satisfies both polarities (readonly narrows).
    let ro_on_rw = AgentPolicySpec {
        binds: vec![bind("/work", "/work", true)],
        ..Default::default()
    };
    assert!(effective(&b, Some(&ro_on_rw), &hardened()).is_ok());
    // Unknown source is rejected — the sandbox.binds list is checked too.
    let mut sandbox = hardened();
    sandbox.binds = vec![bind("/etc", "/etc", true)];
    assert_eq!(
        effective(&b, None, &sandbox),
        Err(not_allowed("/etc", "/etc"))
    );
}

#[test]
fn limits_respect_cap_and_each_field_is_checked() {
    let b = baseline();
    let cap = b.limits_cap;
    let at_cap = AgentSandboxSpec {
        limits: cap,
        ..hardened()
    };
    assert!(effective(&b, None, &at_cap).is_ok());
    // Each field over the cap — and zero (Q63 forbids unbounded limits) —
    // is rejected with the offending field named.
    for (field, over) in [
        ("cpuMillis", limits(cap.cpu_millis + 1, 1 << 20, 64)),
        ("memoryBytes", limits(500, cap.memory_bytes + 1, 64)),
        ("pidsMax", limits(500, 1 << 20, cap.pids_max + 1)),
        ("cpuMillis", limits(0, 1 << 20, 64)),
    ] {
        let sandbox = AgentSandboxSpec {
            limits: over,
            ..hardened()
        };
        assert_eq!(
            effective(&b, None, &sandbox),
            Err(PolicyViolation::LimitsExceedCap(field)),
            "{field} limit must be bounded by the cap"
        );
    }
}

#[test]
fn identity_defaults_nonroot_and_rejects_root() {
    let eff = effective(&baseline(), None, &hardened()).unwrap();
    assert_eq!((eff.uid, eff.gid), (SANDBOX_UID, SANDBOX_GID)); // 1000/1000
    for field in ["uid", "gid"] {
        let mut sandbox = hardened();
        match field {
            "uid" => sandbox.uid = Some(0),
            _ => sandbox.gid = Some(0),
        }
        assert_eq!(
            effective(&baseline(), None, &sandbox),
            Err(PolicyViolation::RootIdentity(field))
        );
    }
    // Admission rejects root identity with the frozen message (uid first).
    let root = agent_body(json!({"sandbox": {"mode": "hardened", "uid": 0, "gid": 0,
        "limits": limits_json()}}));
    assert_eq!(
        admit("init-pro.io", "Agent", &root).unwrap_err(),
        "spec.sandbox.uid: Invalid value: 0: hardened sandbox requires a non-root identity (Q63)"
    );
}

#[test]
fn inherit_is_verbatim() {
    let policy = AgentPolicySpec {
        allow_tools: Some(vec![ToolClass::Ordinary]),
        provider_operations: Some(vec!["files.read".to_string()]),
        binds: vec![bind("/work", "/work", false)],
    };
    let eff = effective(&baseline(), Some(&policy), &hardened()).unwrap();
    let child = inherit(&eff);
    assert_eq!(child, eff);
    // The subagent recurses through the same gate with the same world.
    let GateDecision::Allow = gate(&child, ToolClass::Ordinary, "read") else {
        panic!()
    };
    let GateDecision::Deny(_) = gate(&child, ToolClass::Ssh, "ssh") else {
        panic!()
    };
}

#[test]
fn denial_audit_has_exactly_the_frozen_keys() {
    let audit = denial_audit("agent-1", "attempt-7", ToolClass::Ssh, "ssh.exec", "denied");
    assert_eq!(
        audit,
        json!({
            "type": "ActionDenied", "agent": "agent-1", "attempt": "attempt-7",
            "toolClass": "ssh", "action": "ssh.exec", "reason": "denied"
        })
    );
    // Exactly six camelCase keys — no room for secret material.
    assert_eq!(audit.as_object().unwrap().len(), 6);
}

#[test]
fn effective_rechecks_sandbox_mode_defense_in_depth() {
    let sandbox = AgentSandboxSpec {
        mode: "permissive".to_string(),
        ..hardened()
    };
    let err = effective(&baseline(), None, &sandbox).unwrap_err();
    assert_eq!(
        err,
        PolicyViolation::UnknownSandboxMode("permissive".to_string())
    );
    assert_eq!(
        err.reason(),
        "spec.sandbox.mode: Invalid value: \"permissive\": supported value is \"hardened\" (Q63)"
    );
}

// --- admission (D1) ----------------------------------------------------------

#[test]
fn admission_requires_bounded_limits() {
    let none = agent_body(json!({"sandbox": {"mode": "hardened"}}));
    assert_eq!(
        admit("init-pro.io", "Agent", &none).unwrap_err(),
        "spec.sandbox.limits: Required value: hardened sandbox requires bounded limits \
         (Q63: no unlimited CPU/memory/PIDs)"
    );
    let zero = agent_body(json!({"sandbox": {"mode": "hardened",
        "limits": {"cpuMillis": 0, "memoryBytes": 1024, "pidsMax": 8}}}));
    assert_eq!(
        admit("init-pro.io", "Agent", &zero).unwrap_err(),
        "spec.sandbox.limits.cpuMillis: Required value: hardened sandbox requires bounded \
         limits (Q63: no unlimited CPU/memory/PIDs)"
    );
    // validate_agent_spec takes the spec object directly (validate.rs
    // delegates after extracting body.spec).
    assert!(validate_agent_spec(&json!({"agent": "opencoder"})).is_ok());
}

#[test]
fn admission_rejects_unknown_sandbox_mode() {
    let body = agent_body(json!({"sandbox": {"mode": "permissive", "limits": limits_json()}}));
    assert_eq!(
        admit("init-pro.io", "Agent", &body).unwrap_err(),
        "spec.sandbox.mode: Invalid value: \"permissive\": supported value is \"hardened\" (Q63)"
    );
}

#[test]
fn admission_rejects_invalid_policy_shapes() {
    let unknown_tool = agent_body(json!({"policy": {"allowTools": ["nuclear"]}}));
    let err = admit("init-pro.io", "Agent", &unknown_tool).unwrap_err();
    assert!(
        err.contains("spec.policy.allowTools[0]") && err.contains("nuclear"),
        "{err}"
    );
    let bad_op = agent_body(json!({"policy": {"providerOperations": ["a\nb"]}}));
    let err = admit("init-pro.io", "Agent", &bad_op).unwrap_err();
    assert!(err.contains("spec.policy.providerOperations[0]"), "{err}");
    let relative = agent_body(
        json!({"sandbox": {"mode": "hardened", "limits": limits_json(),
        "binds": [{"source": "rel/path", "target": "/b"}]}}),
    );
    let err = admit("init-pro.io", "Agent", &relative).unwrap_err();
    assert!(err.contains("spec.sandbox.binds[0].source"), "{err}");
    let dotdot = agent_body(json!({"policy": {"binds": [{"source": "/a", "target": "/b/../c"}]}}));
    let err = admit("init-pro.io", "Agent", &dotdot).unwrap_err();
    assert!(err.contains("spec.policy.binds[0].target"), "{err}");
}

#[test]
fn admission_accepts_valid_hardened_agent() {
    let body = agent_body(json!({
        "agent": "opencoder", "prompt": "hi",
        "sandbox": {"mode": "hardened", "uid": 1000, "gid": 1000,
            "limits": limits_json(),
            "binds": [{"source": "/data", "target": "/data", "readonly": true}]},
        "policy": {"allowTools": ["ordinary", "subagent"],
            "providerOperations": ["files.read"],
            "binds": [{"source": "/data", "target": "/data", "readonly": true}]}
    }));
    assert!(admit("init-pro.io", "Agent", &body).is_ok());
    // Group matching stays case-insensitive (registry parity).
    assert!(admit("Init-Pro.io", "Agent", &body).is_ok());
    // Legacy Q37 Agents (no policy/sandbox) and missing specs are untouched.
    let legacy = agent_body(json!({"agent": "opencoder"}));
    assert!(admit("init-pro.io", "Agent", &legacy).is_ok());
    assert!(admit("init-pro.io", "Agent", &json!({"metadata": {"name": "a"}})).is_ok());
}

// --- wire shapes -------------------------------------------------------------

#[test]
fn agent_spec_policy_sandbox_serde_roundtrip() {
    let json = r#"{"agent":"opencoder","prompt":"hi",
        "sandbox":{"mode":"hardened",
        "limits":{"cpuMillis":500,"memoryBytes":1048576,"pidsMax":64}},
        "policy":{"allowTools":["ordinary"],
        "binds":[{"source":"/a","target":"/b","readonly":true}]}}"#;
    let spec: AgentSpec = serde_json::from_str(json).unwrap();
    let sandbox = spec.sandbox.as_ref().expect("sandbox decodes");
    assert_eq!(sandbox.mode, "hardened");
    assert_eq!(sandbox.limits, limits(500, 1048576, 64));
    assert_eq!(sandbox.uid, None);
    let policy = spec.policy.as_ref().expect("policy decodes");
    assert_eq!(
        policy.allow_tools.as_deref(),
        Some(&[ToolClass::Ordinary][..])
    );
    assert_eq!(policy.binds, vec![bind("/a", "/b", true)]);
    assert!(hardened_requested(spec.sandbox.as_ref()));
    assert!(!hardened_requested(None));
    // camelCase keys survive a re-encode (Q10 wire contract).
    let wire = serde_json::to_value(&spec).unwrap();
    assert_eq!(
        wire.pointer("/policy/allowTools"),
        Some(&json!(["ordinary"]))
    );
    assert_eq!(wire.pointer("/sandbox/limits/cpuMillis"), Some(&json!(500)));
}

// --- T7.7 D7: EffectivePolicy serde (the runner<->node wire contract) ---------

#[test]
fn effective_policy_serde_roundtrip_camel_case() {
    let eff = EffectivePolicy {
        tools: BTreeSet::from([ToolClass::Ordinary, ToolClass::Subagent]),
        provider_operations: BTreeSet::from(["files.read".to_string()]),
        binds: vec![bind("/data", "/data", true)],
        limits: limits(500, 268435456, 64),
        uid: SANDBOX_UID,
        gid: SANDBOX_GID,
    };
    let wire = serde_json::to_value(&eff).unwrap();
    // camelCase keys on the wire (Q10) — the hardened agent-runner parses
    // exactly this shape from SANDBOX_POLICY_FILE.
    assert_eq!(
        wire.get("providerOperations"),
        Some(&json!(["files.read"])),
        "wire: {wire}"
    );
    assert_eq!(
        wire.get("limits"),
        Some(&json!({
            "cpuMillis": 500, "memoryBytes": 268435456, "pidsMax": 64
        }))
    );
    let back: EffectivePolicy = serde_json::from_value(wire).unwrap();
    assert_eq!(back, eff);
}
