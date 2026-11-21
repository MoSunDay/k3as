//! Agent action policy + hardened sandbox containment (T7.7, Q63).
//!
//! D1: the wire types ([`AgentPolicySpec`]/[`AgentSandboxSpec`] on
//! `AgentSpec`) + admission [`validate_agent_spec`]; D6: pure node-side
//! evaluation — default-deny [`PolicyBaseline`], narrowing-only
//! [`effective`], tool-dispatch [`gate`], verbatim subagent [`inherit`],
//! secret-free [`denial_audit`]. Widening is never clamped, only
//! deterministically rejected (Q10 JSON; tests: `agent_policy_tests`).

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The only supported sandbox mode (Q63 hardened containment).
pub const SANDBOX_MODE_HARDENED: &str = "hardened";
/// Default non-root uid for the hardened sandbox (Q63).
pub const SANDBOX_UID: u32 = 1000;
/// Default non-root gid for the hardened sandbox (Q63).
pub const SANDBOX_GID: u32 = 1000;
/// Where the node writes the attempt's `EffectivePolicy` inside the
/// sandbox workspace (T7.7 D7: the hardened `agent-runner` reads it
/// fail-closed; content is policy, never a secret).
pub const SANDBOX_POLICY_FILE: &str = "/workspace/.init-pro/policy.json";
/// Where the hardened `agent-runner` appends denial audit JSONL inside
/// the sandbox workspace (T7.7 D7; advisory — agent-tamperable by design,
/// the node-side verdict stays authoritative).
pub const SANDBOX_AUDIT_FILE: &str = "/workspace/.init-pro/audit.jsonl";

/// Tool classes routed through the central tool-dispatch gate (Q63).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolClass {
    Ordinary,
    Custom,
    Mcp,
    Ssh,
    Subagent,
}

impl ToolClass {
    /// Every known class, in declaration order.
    pub const ALL: [ToolClass; 5] = [
        ToolClass::Ordinary,
        ToolClass::Custom,
        ToolClass::Mcp,
        ToolClass::Ssh,
        ToolClass::Subagent,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            ToolClass::Ordinary => "ordinary",
            ToolClass::Custom => "custom",
            ToolClass::Mcp => "mcp",
            ToolClass::Ssh => "ssh",
            ToolClass::Subagent => "subagent",
        }
    }

    /// Parse a wire name; `None` for unknown strings.
    pub fn parse(s: &str) -> Option<ToolClass> {
        ToolClass::ALL.into_iter().find(|c| c.as_str() == s)
    }
}

/// One selective host bind (operator-allowlisted; specs may only narrow): absolute `source` host path, absolute `target` mount point, `readonly` request — a read-only allowlist slot only satisfies read-only requests.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BindRule {
    pub source: String,
    pub target: String,
    #[serde(default)]
    pub readonly: bool,
}

/// Mandatory bounded cgroup limits for the hardened path (Q63): cpuMillis / memoryBytes / pidsMax — every field must be positive and within the operator cap (no unlimited CPU/memory/PIDs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SandboxLimits {
    pub cpu_millis: u64,
    pub memory_bytes: u64,
    pub pids_max: u64,
}

/// Spec-level action policy — narrowing-only overlay on the operator baseline: an absent field inherits the baseline verbatim, a present field may only request a subset (widening is deterministically rejected, Q63).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentPolicySpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_tools: Option<Vec<ToolClass>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_operations: Option<Vec<String>>,
    /// Requested host binds (checked against the operator allowlist).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub binds: Vec<BindRule>,
}

/// Spec-level sandbox containment request (Q63 hardened mode; absent = the legacy Q37 path): `mode` must be "hardened", `uid`/`gid` default to the non-root [`SANDBOX_UID`]/[`SANDBOX_GID`] (explicit 0 is rejected), `limits` are REQUIRED.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSandboxSpec {
    pub mode: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gid: Option<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub binds: Vec<BindRule>,
    pub limits: SandboxLimits,
}

/// Operator-owned default-deny baseline (Q63): denies everything unless the operator explicitly allows it (tools, provider operations, bind slots) and caps the limits any Agent on this node may request.
#[derive(Debug, Clone, PartialEq)]
pub struct PolicyBaseline {
    pub tools: BTreeSet<ToolClass>,
    pub provider_operations: BTreeSet<String>,
    pub bind_allowlist: Vec<BindRule>,
    pub limits_cap: SandboxLimits,
}

impl PolicyBaseline {
    /// The maximally denying baseline: nothing allowed, zero cap — under it no hardened sandbox can be effective (every positive limit exceeds the cap), which is the Q63 intent.
    pub fn deny_all() -> Self {
        PolicyBaseline {
            tools: BTreeSet::new(),
            provider_operations: BTreeSet::new(),
            bind_allowlist: Vec::new(),
            limits_cap: SandboxLimits {
                cpu_millis: 0,
                memory_bytes: 0,
                pids_max: 0,
            },
        }
    }
}

/// Deterministic, auditable violation (no secrets ever — Q63).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyViolation {
    WidenedTool(ToolClass),
    UnknownToolClass(String),
    WidenedProviderOp(String),
    BindNotAllowlisted { source: String, target: String },
    LimitsExceedCap(&'static str),
    RootIdentity(&'static str),
    UnknownSandboxMode(String),
}

/// `spec.policy.<field>: Forbidden: <what> is not in the operator baseline (Q63: policies may only narrow)`.
fn widen_msg(field: &str, what: &str) -> String {
    format!("spec.policy.{field}: Forbidden: {what} is not in the operator baseline (Q63: policies may only narrow)")
}

impl PolicyViolation {
    /// Stable, actionable, deterministic reason string (Q63).
    pub fn reason(&self) -> String {
        match self {
            Self::WidenedTool(c) => widen_msg("allowTools", &format!("class \"{}\"", c.as_str())),
            Self::UnknownToolClass(raw) => {
                format!("tool class \"{raw}\": Invalid value: expected ordinary|custom|mcp|ssh|subagent (Q63)")
            }
            Self::WidenedProviderOp(op) => widen_msg("providerOperations", &format!("op \"{op}\"")),
            Self::BindNotAllowlisted { source, target } => {
                format!("binds: Forbidden: {source}->{target} is not on the operator bind allowlist (Q63)")
            }
            Self::LimitsExceedCap(f) => {
                format!("spec.sandbox.limits.{f}: Forbidden: must be positive and within the operator cap (Q63)")
            }
            Self::RootIdentity(f) => {
                format!("spec.sandbox.{f}: Invalid value: 0: hardened sandbox requires a non-root identity (Q63)")
            }
            Self::UnknownSandboxMode(m) => {
                format!("spec.sandbox.mode: Invalid value: \"{m}\": supported value is \"hardened\" (Q63)")
            }
        }
    }
}

/// The effective policy an attempt (and every subagent, verbatim) runs under: narrowed tools + provider operations, allowlisted binds, enforced limits, resolved non-root identity.
///
/// Serde (camelCase, T7.7 D7): the node serializes this into the sandbox
/// at `SANDBOX_POLICY_FILE` and the hardened `agent-runner` deserializes
/// it fail-closed — the wire shape is the Q63 contract between them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EffectivePolicy {
    pub tools: BTreeSet<ToolClass>,
    pub provider_operations: BTreeSet<String>,
    pub binds: Vec<BindRule>,
    pub limits: SandboxLimits,
    pub uid: u32,
    pub gid: u32,
}

/// True when the Agent carries any `spec.sandbox` (any mode value counts as requested; mode validity is checked by admission / `effective`).
pub fn hardened_requested(sandbox: Option<&AgentSandboxSpec>) -> bool {
    sandbox.is_some()
}

/// Evaluate the effective policy for the hardened path (D6): intersect the
/// spec overlay with the operator baseline — absent overlay fields inherit the baseline, every present field may only narrow. Check order is fixed for determinism: mode, uid, gid, tools, provider ops, binds, limits.
pub fn effective(
    baseline: &PolicyBaseline,
    policy: Option<&AgentPolicySpec>,
    sandbox: &AgentSandboxSpec,
) -> Result<EffectivePolicy, PolicyViolation> {
    if sandbox.mode != SANDBOX_MODE_HARDENED {
        return Err(PolicyViolation::UnknownSandboxMode(sandbox.mode.clone()));
    }
    let uid = sandbox.uid.unwrap_or(SANDBOX_UID);
    let gid = sandbox.gid.unwrap_or(SANDBOX_GID);
    let root = if uid == 0 { "uid" } else { "gid" };
    if uid == 0 || gid == 0 {
        return Err(PolicyViolation::RootIdentity(root));
    }
    let tools = match policy.and_then(|p| p.allow_tools.as_ref()) {
        None => baseline.tools.clone(),
        Some(requested) => {
            for class in requested {
                if !baseline.tools.contains(class) {
                    return Err(PolicyViolation::WidenedTool(*class));
                }
            }
            requested.iter().copied().collect()
        }
    };
    let provider_operations = match policy.and_then(|p| p.provider_operations.as_ref()) {
        None => baseline.provider_operations.clone(),
        Some(requested) => {
            for op in requested {
                if !baseline.provider_operations.contains(op.as_str()) {
                    return Err(PolicyViolation::WidenedProviderOp(op.clone()));
                }
            }
            requested.iter().cloned().collect()
        }
    };
    let mut binds: Vec<BindRule> = policy.map(|p| p.binds.clone()).unwrap_or_default();
    binds.extend(sandbox.binds.iter().cloned());
    for rule in &binds {
        // A read-only allowlist slot satisfies only read-only requests; a
        // read-write slot satisfies both polarities (readonly narrows).
        let matched = |s: &BindRule| {
            rule.source == s.source && rule.target == s.target && (rule.readonly || !s.readonly)
        };
        if !baseline.bind_allowlist.iter().any(matched) {
            return Err(PolicyViolation::BindNotAllowlisted {
                source: rule.source.clone(),
                target: rule.target.clone(),
            });
        }
    }
    let limits = sandbox.limits;
    let cap = baseline.limits_cap;
    if limits.cpu_millis == 0 || limits.cpu_millis > cap.cpu_millis {
        return Err(PolicyViolation::LimitsExceedCap("cpuMillis"));
    }
    if limits.memory_bytes == 0 || limits.memory_bytes > cap.memory_bytes {
        return Err(PolicyViolation::LimitsExceedCap("memoryBytes"));
    }
    if limits.pids_max == 0 || limits.pids_max > cap.pids_max {
        return Err(PolicyViolation::LimitsExceedCap("pidsMax"));
    }
    Ok(EffectivePolicy {
        tools,
        provider_operations,
        binds,
        limits,
        uid,
        gid,
    })
}

/// Subagents inherit the parent effective policy VERBATIM (Q63) and recurse through the same gate.
pub fn inherit(eff: &EffectivePolicy) -> EffectivePolicy {
    eff.clone()
}

/// Central tool-dispatch gate decision (Q63): `Deny` carries a deterministic reason (class + action name; tool names are not secrets).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateDecision {
    Allow,
    Deny(String),
}

/// Allow iff `class` is in the effective tool set; the denial reason carries the class and the action name (deterministic, auditable).
pub fn gate(eff: &EffectivePolicy, class: ToolClass, action: &str) -> GateDecision {
    if eff.tools.contains(&class) {
        GateDecision::Allow
    } else {
        GateDecision::Deny(format!(
            "tool class \"{}\" is denied by the agent action policy (action: \"{action}\") (Q63)",
            class.as_str()
        ))
    }
}

/// Audit event for a denial; contains NO secrets by construction (only ids/class/reason — Q63). Exactly the camelCase keys type/agent/attempt/toolClass/action/reason.
pub fn denial_audit(
    agent: &str,
    attempt: &str,
    class: ToolClass,
    action: &str,
    reason: &str,
) -> Value {
    serde_json::json!({
        "type": "ActionDenied",
        "agent": agent,
        "attempt": attempt,
        "toolClass": class.as_str(),
        "action": action,
        "reason": reason,
    })
}

const LIMITS_MSG: &str =
    "Required value: hardened sandbox requires bounded limits (Q63: no unlimited CPU/memory/PIDs)";

/// Admission validation of an Agent `spec` (D1): raw-JSON shape checks for the Q63 fields; called from `validate::admit` (a missing spec object has nothing to check). Narrowing vs the operator baseline is a node-side `effective` concern — admission rejects invalid shapes only.
pub fn validate_agent_spec(spec: &Value) -> Result<(), String> {
    if let Some(sandbox) = spec.get("sandbox") {
        validate_sandbox_json(sandbox)?;
    }
    if let Some(policy) = spec.get("policy") {
        validate_policy_json(policy)?;
    }
    Ok(())
}

/// Enumerated elements of an optional JSON array (missing / non-array = none).
fn items(v: Option<&Value>) -> impl Iterator<Item = (usize, &Value)> {
    v.and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
}

fn validate_sandbox_json(sandbox: &Value) -> Result<(), String> {
    let mode = sandbox.get("mode").and_then(Value::as_str);
    if mode != Some(SANDBOX_MODE_HARDENED) {
        return Err(match mode {
            Some(other) => {
                format!("spec.sandbox.mode: Invalid value: \"{other}\": supported value is \"hardened\" (Q63)")
            }
            None => "spec.sandbox.mode: Required value: supported value is \"hardened\" (Q63)"
                .to_string(),
        });
    }
    for field in ["uid", "gid"] {
        if sandbox.get(field).and_then(Value::as_u64) == Some(0) {
            return Err(format!(
                "spec.sandbox.{field}: Invalid value: 0: hardened sandbox requires a non-root \
                 identity (Q63)"
            ));
        }
    }
    let Some(limits) = sandbox.get("limits") else {
        return Err(format!("spec.sandbox.limits: {LIMITS_MSG}"));
    };
    for field in ["cpuMillis", "memoryBytes", "pidsMax"] {
        if limits.get(field).and_then(Value::as_u64).unwrap_or(0) == 0 {
            return Err(format!("spec.sandbox.limits.{field}: {LIMITS_MSG}"));
        }
    }
    validate_binds_json(sandbox.get("binds"), "spec.sandbox.binds")
}

fn validate_policy_json(policy: &Value) -> Result<(), String> {
    for (i, raw) in items(policy.get("allowTools")) {
        if raw.as_str().and_then(ToolClass::parse).is_none() {
            return Err(format!(
                "spec.policy.allowTools[{i}]: Invalid value: {raw}: unsupported tool class: \
                 expected one of ordinary, custom, mcp, ssh, subagent (Q63)"
            ));
        }
    }
    for (i, op) in items(policy.get("providerOperations")) {
        let name = op.as_str().unwrap_or("");
        if name.is_empty() || name.chars().any(char::is_control) {
            return Err(format!(
                "spec.policy.providerOperations[{i}]: Invalid value: {op}: must be a non-empty \
                 name without control characters (Q63)"
            ));
        }
    }
    validate_binds_json(policy.get("binds"), "spec.policy.binds")
}

/// Bind-shape rule shared by `policy.binds` and `sandbox.binds`: `source` and `target` must be absolute paths without ".." components.
fn validate_binds_json(binds: Option<&Value>, path: &str) -> Result<(), String> {
    for (i, bind) in items(binds) {
        for field in ["source", "target"] {
            let v = bind.get(field).and_then(Value::as_str).unwrap_or("");
            if !(v.starts_with('/') && v.split('/').all(|c| c != "..")) {
                return Err(format!(
                    "{path}[{i}].{field}: Invalid value: \"{v}\": must be an absolute path \
                     without \"..\" components (Q63)"
                ));
            }
        }
    }
    Ok(())
}
