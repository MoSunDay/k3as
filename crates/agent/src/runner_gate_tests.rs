//! T7.7 D7 (Q63) unit evidence for `runner_gate`: class mapping,
//! advertisement exclusion, gate denial determinism (+ the secret-free
//! audit line), allowed dispatch passthrough, and full-registry wrapping.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use opencoder_core::{Tool, ToolArc, ToolContext, ToolOutput};
use serde_json::{json, Value};

use crate::runner_gate::{advertised_tools, tool_class_of, wrap_registry, ALL_TOOL_NAMES};
use api::agent_policy::{EffectivePolicy, SandboxLimits, ToolClass, SANDBOX_AUDIT_FILE};

fn eff(classes: &[ToolClass]) -> EffectivePolicy {
    EffectivePolicy {
        tools: classes.iter().copied().collect::<BTreeSet<_>>(),
        provider_operations: BTreeSet::new(),
        binds: Vec::new(),
        limits: SandboxLimits {
            cpu_millis: 500,
            memory_bytes: 268435456,
            pids_max: 64,
        },
        uid: 1000,
        gid: 1000,
    }
}

/// Fake inner tool: records its invocation input into the shared map.
struct RecordingTool {
    name: String,
    seen: std::sync::Arc<std::sync::Mutex<BTreeMap<String, Value>>>,
}

#[async_trait]
impl Tool for RecordingTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        "recording fake"
    }
    fn parameters(&self) -> Value {
        json!({"type": "object"})
    }
    async fn execute(&self, input: Value, _ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
        self.seen
            .lock()
            .unwrap()
            .insert(self.name.clone(), input.clone());
        Ok(ToolOutput::ok(format!("ran {}", self.name)))
    }
}

fn audit_tmp(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!(
        "init-pro-gate-{tag}-{}-{}",
        std::process::id(),
        tag.len()
    ));
    let _ = std::fs::remove_file(&p);
    p
}

fn ctx() -> ToolContext {
    ToolContext {
        session_id: "s".into(),
        message_id: "m".into(),
        agent: "act".into(),
        working_dir: PathBuf::from("/tmp"),
        max_output: 4096,
        proxy: None,
        tools_path: None,
    }
}

#[test]
fn class_mapping_is_total_and_deterministic() {
    assert_eq!(tool_class_of("task"), ToolClass::Subagent);
    assert_eq!(tool_class_of("ssh_pty"), ToolClass::Ssh);
    assert_eq!(tool_class_of("mcp__github__issue"), ToolClass::Mcp);
    for n in [
        "bash",
        "read",
        "view_image",
        "edit",
        "search",
        "ls",
        "question",
    ] {
        assert_eq!(tool_class_of(n), ToolClass::Ordinary, "{n}");
    }
    // Custom + hallucinated names are never exempt (Q63 default-deny).
    assert_eq!(tool_class_of("delete_everything"), ToolClass::Custom);
    assert_eq!(tool_class_of(""), ToolClass::Custom);
}

#[tokio::test]
async fn denied_class_errs_audits_and_never_echoes_input() {
    let seen = std::sync::Arc::new(std::sync::Mutex::new(BTreeMap::new()));
    let mut base = HashMap::new();
    base.insert(
        "read".to_string(),
        std::sync::Arc::new(RecordingTool {
            name: "read".into(),
            seen: seen.clone(),
        }) as ToolArc,
    );
    let audit = audit_tmp("deny");
    let gated = wrap_registry(
        base,
        &eff(&[ToolClass::Subagent]), // Ordinary denied
        "ag-1",
        "att-1",
        &audit,
    );
    let out = gated["read"]
        .execute(json!({"path": "sk-canary-xyz"}), &ctx())
        .await
        .unwrap();
    assert!(out.is_error);
    assert!(
        out.content.contains("denied by the agent action policy"),
        "content: {}",
        out.content
    );
    // The inner tool never ran.
    assert!(seen.lock().unwrap().is_empty(), "inner must not execute");
    // Exactly one audit line with the exact camelCase keys and NO input echo.
    let raw = std::fs::read_to_string(&audit).unwrap();
    let lines: Vec<&str> = raw.lines().collect();
    assert_eq!(lines.len(), 1, "raw: {raw}");
    let v: Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(v.get("type"), Some(&json!("ActionDenied")));
    assert_eq!(v.get("agent"), Some(&json!("ag-1")));
    assert_eq!(v.get("attempt"), Some(&json!("att-1")));
    assert_eq!(v.get("toolClass"), Some(&json!("ordinary")));
    assert_eq!(v.get("action"), Some(&json!("read")));
    assert!(v.get("reason").is_some_and(|r| r.is_string()));
    assert_eq!(
        v.as_object().unwrap().len(),
        6,
        "exact key set, nothing else: {v}"
    );
    // The canary (input material) must appear nowhere.
    assert!(!raw.contains("sk-canary-xyz"));
    assert!(!out.content.contains("sk-canary-xyz"));
    let _ = std::fs::remove_file(&audit);
}

#[tokio::test]
async fn allowed_class_delegates_to_inner() {
    let seen = std::sync::Arc::new(std::sync::Mutex::new(BTreeMap::new()));
    let mut base = HashMap::new();
    base.insert(
        "bash".to_string(),
        std::sync::Arc::new(RecordingTool {
            name: "bash".into(),
            seen: seen.clone(),
        }) as ToolArc,
    );
    let audit = audit_tmp("allow");
    let gated = wrap_registry(base, &eff(&[ToolClass::Ordinary]), "ag-2", "att-2", &audit);
    let out = gated["bash"]
        .execute(json!({"command": "true"}), &ctx())
        .await
        .unwrap();
    assert!(!out.is_error);
    assert_eq!(out.content, "ran bash");
    assert!(seen.lock().unwrap().contains_key("bash"));
    // No denial => no audit file content.
    assert!(!audit.exists() || std::fs::read_to_string(&audit).unwrap().trim().is_empty());
    let _ = std::fs::remove_file(&audit);
}

#[test]
fn wrap_registry_covers_every_key() {
    let names = ["read", "bash", "task", "ssh_pty", "mcp__x__y", "weird"];
    let mut base = HashMap::new();
    for n in names {
        base.insert(
            n.to_string(),
            std::sync::Arc::new(RecordingTool {
                name: n.into(),
                seen: std::sync::Arc::new(std::sync::Mutex::new(BTreeMap::new())),
            }) as ToolArc,
        );
    }
    let gated = wrap_registry(
        base,
        &eff(&[]),
        "ag-3",
        "att-3",
        Path::new(SANDBOX_AUDIT_FILE),
    );
    assert_eq!(gated.len(), names.len());
    for n in names {
        assert!(gated.contains_key(n), "{n} must be wrapped");
        // Schema surfaces delegate verbatim.
        assert_eq!(gated[n].name(), n);
    }
}

#[test]
fn advertised_tools_exclude_denied_classes_and_stay_subset() {
    // Subagent denied => task absent; Ssh denied => ssh_pty absent.
    let no_sub = advertised_tools(&ALL_TOOL_NAMES, &eff(&[ToolClass::Ordinary]));
    assert!(!no_sub.contains(&"task".to_string()));
    assert!(no_sub.contains(&"read".to_string()));
    let no_ssh = advertised_tools(
        &ALL_TOOL_NAMES,
        &eff(&[ToolClass::Ordinary, ToolClass::Subagent]),
    );
    assert!(!no_ssh.contains(&"ssh_pty".to_string()));
    let both = advertised_tools(
        &ALL_TOOL_NAMES,
        &eff(&[ToolClass::Ordinary, ToolClass::Ssh, ToolClass::Subagent]),
    );
    assert!(both.contains(&"task".to_string()) && both.contains(&"ssh_pty".to_string()));
    // Subset property over the full name set.
    for list in [&no_sub, &no_ssh, &both] {
        assert!(list.iter().all(|n| ALL_TOOL_NAMES.contains(&n.as_str())));
    }
}
