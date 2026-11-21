//! T7.7 D7 (Q63): the in-process tool-dispatch gate for the hardened
//! `agent-runner` binary.
//!
//! [`wrap_registry`] wraps EVERY registry entry (including hypothetical
//! `mcp__*` tools and hallucinated out-of-schema names routed through the
//! same map) in [`GatedTool`], so every tool dispatch flows through
//! [`api::agent_policy::gate`] with a deterministic denial + a secret-free
//! audit line. Subagent children inherit the WRAPPED registry verbatim
//! (Q63 verbatim inheritance: the parent map is passed as-is into child
//! sessions, so descendants recurse through the same gate).
//!
//! The parent-side `task` spawn intercept is policy-excluded at
//! advertisement ([`advertised_tools`] drops `task` when the `Subagent`
//! class is denied, so it never appears in the schema); any forced spawn
//! still lands in a fully-gated, cgroup-bounded child.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use opencoder_core::{Tool, ToolArc, ToolContext, ToolOutput};
use serde_json::Value;

use api::agent_policy::{denial_audit, gate, EffectivePolicy, GateDecision, ToolClass};

/// The builtin tool registry surface (schema + gate key space, D7).
pub const ALL_TOOL_NAMES: [&str; 9] = [
    "bash",
    "read",
    "view_image",
    "edit",
    "search",
    "ls",
    "question",
    "task",
    "ssh_pty",
];

/// Map a tool name onto its policy class (Q63): `task` spawns subagents,
/// `ssh_pty` reaches the network, `mcp__*` are external servers, the
/// ordinary file/Q&A builtins are `Ordinary`, everything else (custom +
/// hallucinated names) is `Custom` — never exempt.
pub fn tool_class_of(name: &str) -> ToolClass {
    match name {
        "task" => ToolClass::Subagent,
        "ssh_pty" => ToolClass::Ssh,
        _ if name.starts_with("mcp__") => ToolClass::Mcp,
        "bash" | "read" | "view_image" | "edit" | "search" | "ls" | "question" => {
            ToolClass::Ordinary
        }
        _ => ToolClass::Custom,
    }
}

/// The advertised (schema-visible) tool names: every name whose class is
/// allowed by the effective policy. Denied classes are not advertised —
/// a denied `Subagent` class removes `task` from the schema entirely.
pub fn advertised_tools(names: &[&str], eff: &EffectivePolicy) -> Vec<String> {
    names
        .iter()
        .filter(|n| eff.tools.contains(&tool_class_of(n)))
        .map(|n| n.to_string())
        .collect()
}

/// Best-effort denial audit append: one JSONL line at `audit_path`
/// ([`api::agent_policy::SANDBOX_AUDIT_FILE` convention]). Failures are
/// ignored by design — the audit is advisory; the denial itself is the
/// enforcement (Q63).
fn append_audit(audit_path: &Path, line: &Value) {
    use std::io::Write;
    let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(audit_path)
    else {
        return;
    };
    let _ = writeln!(f, "{line}");
}

/// A [`Tool`] whose every `execute` passes through the central gate first.
/// Name/description/parameters delegate to the inner tool verbatim (the
/// schema surface stays honest); only dispatch is intercepted.
pub struct GatedTool {
    inner: ToolArc,
    eff: EffectivePolicy,
    agent: String,
    attempt: String,
    audit_path: PathBuf,
}

#[async_trait]
impl Tool for GatedTool {
    fn name(&self) -> &str {
        self.inner.name()
    }
    fn description(&self) -> &str {
        self.inner.description()
    }
    fn parameters(&self) -> Value {
        self.inner.parameters()
    }
    async fn execute(&self, input: Value, ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
        let name = self.inner.name().to_string();
        let class = tool_class_of(&name);
        match gate(&self.eff, class, &name) {
            GateDecision::Allow => self.inner.execute(input, ctx).await,
            GateDecision::Deny(msg) => {
                append_audit(
                    &self.audit_path,
                    &denial_audit(&self.agent, &self.attempt, class, &name, &msg),
                );
                Ok(ToolOutput::err(msg))
            }
        }
    }
}

/// Wrap EVERY entry of `base` (including `mcp__*` if ever present) in the
/// gate: the returned map is a drop-in registry replacement. The inner
/// [`ToolArc`] is cloned (shared, never rebuilt); the caller keeps
/// ownership of `base` if it still needs it.
pub fn wrap_registry(
    base: HashMap<String, ToolArc>,
    eff: &EffectivePolicy,
    agent: &str,
    attempt: &str,
    audit_path: &Path,
) -> HashMap<String, ToolArc> {
    base.into_iter()
        .map(|(name, inner)| {
            let gated = GatedTool {
                inner,
                eff: eff.clone(),
                agent: agent.to_string(),
                attempt: attempt.to_string(),
                audit_path: audit_path.to_path_buf(),
            };
            (name, std::sync::Arc::new(gated) as ToolArc)
        })
        .collect()
}
