//! Pure seam for node-side `Agent` execution (TODO **T7.1** Phase B / B1,
//! decisions **Q35** + **Q37**): view extraction from raw JSON, the
//! subprocess argv/env mapping, status merge-patch payloads, and lifecycle
//! transition decisions. NO I/O -- everything here is unit-testable without
//! a server, a mount, or a runner binary.
//!
//! Two locked conventions shape this module:
//! - **Q35**: credentials never enter the spec. The runner only ever reads
//!   `OPENAI_API_KEY`-style vars from ITS OWN inherited environment; this
//!   module merely decides WHICH paths/dirs the subprocess sees.
//! - **Q37 directory layout** (anyhome export `/agent/<name>`, mounted by
//!   the runner): `prompt.md` (persona override), `skills/`, `tools/`,
//!   `mem/`, optional `AGENTS.md`.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::objects::pod_key as object_key;

/// The workspace contract annotation (SSOT: `common::workspace`; kubelet
/// cannot depend on controllers, so the key is asserted equal in tests).
pub const WORKSPACE_ANNOTATION: &str = "init-pro.io/workspace";

/// Agent `status.phase` values (mirror of `api::agent`; Pod-lifecycle parity).
pub const PHASE_PENDING: &str = "Pending";
pub const PHASE_RUNNING: &str = "Running";
pub const PHASE_SUCCEEDED: &str = "Succeeded";
pub const PHASE_FAILED: &str = "Failed";

/// The runner-owned condition type (the scheduler owns `Scheduled`).
pub const COND_READY: &str = "Ready";

/// The flattened, node-relevant view of one Agent object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentView {
    /// `"namespace/name"`.
    pub key: String,
    pub name: String,
    pub namespace: String,
    /// Node binding (`spec.nodeName`); `None` = not yet scheduled.
    pub node_name: Option<String>,
    /// `status.phase`, defaulting to `Pending` when absent (api parity).
    pub phase: String,
    /// The task payload (`spec.prompt`); empty => `EmptyPrompt` fast-fail.
    pub prompt: String,
    /// `spec.model` override (`provider/model`), optional.
    pub model: Option<String>,
    /// `spec.agent` preset override (act/build/explore/sandbox), optional.
    pub agent: Option<String>,
}

/// `"namespace/name"` (same spelling as pods).
pub fn agent_key(v: &Value) -> Option<String> {
    object_key(v)
}

/// The bound node (`spec.nodeName`), if any.
pub fn agent_node(v: &Value) -> Option<String> {
    v.pointer("/spec/nodeName")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Extract the [`AgentView`] (name/namespace must exist; phase defaults).
pub fn agent_view(v: &Value) -> Option<AgentView> {
    let name = v.pointer("/metadata/name")?.as_str()?.to_string();
    let namespace = v
        .pointer("/metadata/namespace")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or("default")
        .to_string();
    Some(AgentView {
        key: format!("{namespace}/{name}"),
        node_name: agent_node(v),
        phase: v
            .pointer("/status/phase")
            .and_then(Value::as_str)
            .unwrap_or(PHASE_PENDING)
            .to_string(),
        prompt: v
            .pointer("/spec/prompt")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        model: v
            .pointer("/spec/model")
            .and_then(Value::as_str)
            .map(str::to_string),
        agent: v
            .pointer("/spec/agent")
            .and_then(Value::as_str)
            .map(str::to_string),
        name,
        namespace,
    })
}

/// Parse the workspace contract annotation into `(addr, export_path)`:
/// `nfs://<addr>/agent/<name>` -> `("<addr>", "/agent/<name>")` (Q36/Q37).
pub fn workspace_nfs_source(v: &Value) -> Option<(String, String)> {
    let raw = v
        .pointer("/metadata/annotations")
        .and_then(|a| a.get(WORKSPACE_ANNOTATION))
        .and_then(Value::as_str)?;
    let rest = raw.strip_prefix("nfs://")?;
    let (addr, path) = rest.split_once('/')?;
    if addr.is_empty() {
        return None;
    }
    Some((addr.to_string(), format!("/{path}")))
}

/// Terminal fast-fail before spawning (Q37): an empty task prompt is a
/// deterministic misconfiguration, not a hang.
pub fn validate_prompt(view: &AgentView) -> Result<(), &'static str> {
    if view.prompt.trim().is_empty() {
        Err("EmptyPrompt")
    } else {
        Ok(())
    }
}

/// Everything needed to spawn one runner subprocess.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnPlan {
    pub args: Vec<String>,
    /// ADDITIONAL env (inherited process env is kept untouched so the
    /// operator-injected key flows through without ever touching the spec).
    pub envs: Vec<(String, String)>,
    pub workdir: PathBuf,
}

/// Build the headless one-shot invocation (`opencoder run "<prompt>"`).
///
/// Env mapping (Q37): `HOME` = workspace (redirects `~/.opencoder` config +
/// skills + domain files), `XDG_DATA_HOME` = `<workspace>/mem` (session DB
/// persistence), `PATH` = `<workspace>/tools` prepended (operator-shipped
/// tools). `--prompt-file` is passed ONLY when the persona file exists
/// (spec prompt stays the positional task; the file overrides the persona).
pub fn build_spawn_plan(
    view: &AgentView,
    workspace: &Path,
    prompt_file: Option<PathBuf>,
    inherited_path: Option<&str>,
) -> SpawnPlan {
    let mut args: Vec<String> = vec!["run".to_string()];
    args.push("--workdir".to_string());
    args.push(workspace.display().to_string());
    if let Some(m) = &view.model {
        args.push("--model".to_string());
        args.push(m.clone());
    }
    if let Some(a) = &view.agent {
        args.push("--agent".to_string());
        args.push(a.clone());
    }
    if let Some(pf) = &prompt_file {
        args.push("--prompt-file".to_string());
        args.push(pf.display().to_string());
    }
    args.push(view.prompt.clone());
    let data_home = workspace.join("mem");
    let mut envs = vec![
        ("HOME".to_string(), workspace.display().to_string()),
        ("XDG_DATA_HOME".to_string(), data_home.display().to_string()),
    ];
    let path = match inherited_path {
        Some(p) => format!("{}/tools:{p}", workspace.display()),
        None => format!("{}/tools", workspace.display()),
    };
    envs.push(("PATH".to_string(), path));
    SpawnPlan {
        args,
        envs,
        workdir: workspace.to_path_buf(),
    }
}

/// Where the workspace for `name` lives on THIS node: anyhome NFS subpath
/// mount when a source resolves (annotation first, then the runner's
/// anyhome address), else a node-local degraded dir (Q37: memory is then
/// node-local; the warning is logged by the caller).
pub fn workspace_source(
    view_value: &Value,
    anyhome_addr: Option<&str>,
) -> Option<(String, String)> {
    workspace_nfs_source(view_value).or_else(|| {
        let path = common::workspace::agent_workspace_path(
            view_value
                .pointer("/metadata/name")
                .and_then(Value::as_str)
                .unwrap_or_default(),
        )?;
        anyhome_addr.map(|a| (a.to_string(), path))
    })
}

/// A merge-patch body carrying ONLY the status (agents have no `/status`
/// subresource; the apiserver RFC 7386-merges the main resource, Q37).
pub fn status_patch(phase: &str, conditions: &[Value]) -> Value {
    json!({"status": {"phase": phase, "conditions": conditions}})
}

/// One `meta/v1.Condition`-shaped entry (mirror of `api::agent::AgentCondition`).
pub fn condition(type_: &str, status: &str, reason: &str, message: &str) -> Value {
    json!({"type": type_, "status": status, "reason": reason, "message": message})
}

/// How the runner subprocess ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitOutcome {
    /// Exited with a code.
    Code(i32),
    /// Killed by the runner (timeout, shutdown).
    Killed,
}

/// Map an exit outcome to the terminal phase (Pod parity: 0 => Succeeded).
pub fn terminal_phase(outcome: ExitOutcome) -> &'static str {
    match outcome {
        ExitOutcome::Code(0) => PHASE_SUCCEEDED,
        ExitOutcome::Code(_) | ExitOutcome::Killed => PHASE_FAILED,
    }
}

/// Best-effort session-id extraction from the runner's stderr tail.
/// opencoder prints `[session <id>]` to stderr on headless completion.
pub fn session_id_from_stderr(stderr: &str) -> Option<String> {
    let start = stderr.find("[session ")? + "[session ".len();
    let rest = &stderr[start..];
    let end = rest.find(']')?;
    let id = rest[..end].trim();
    if id.is_empty() {
        None
    } else {
        Some(id.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(prompt: &str) -> Value {
        serde_json::json!({
            "apiVersion": "init-pro.io/v1", "kind": "Agent",
            "metadata": {"name": "ag-1", "namespace": "default"},
            "spec": {"prompt": prompt}
        })
    }

    #[test]
    fn view_extracts_fields_and_defaults_phase() {
        let v = agent_view(&agent("do it")).unwrap();
        assert_eq!(v.key, "default/ag-1");
        assert_eq!(v.node_name, None);
        assert_eq!(v.phase, PHASE_PENDING);
        assert_eq!(v.prompt, "do it");
        let mut bound = agent("x");
        bound["spec"]["nodeName"] = json!("n1");
        bound["status"] = json!({"phase": "Running"});
        bound["spec"]["model"] = json!("openai/gpt");
        bound["spec"]["agent"] = json!("act");
        let v = agent_view(&bound).unwrap();
        assert_eq!(v.node_name.as_deref(), Some("n1"));
        assert_eq!(v.phase, PHASE_RUNNING);
        assert_eq!(v.model.as_deref(), Some("openai/gpt"));
        assert_eq!(v.agent.as_deref(), Some("act"));
    }

    #[test]
    fn workspace_annotation_parses_nfs_url() {
        let mut a = agent("x");
        a["metadata"]["annotations"] =
            json!({WORKSPACE_ANNOTATION: "nfs://127.0.0.1:11111/agent/ag-1"});
        assert_eq!(
            workspace_nfs_source(&a),
            Some(("127.0.0.1:11111".into(), "/agent/ag-1".into()))
        );
        // Garbage shapes are None, never a panic.
        a["metadata"]["annotations"] = json!({WORKSPACE_ANNOTATION: "http://x/y"});
        assert_eq!(workspace_nfs_source(&a), None);
        a["metadata"]["annotations"] = json!({WORKSPACE_ANNOTATION: "nfs:///agent/x"});
        assert_eq!(workspace_nfs_source(&a), None);
        assert_eq!(workspace_nfs_source(&agent("x")), None);
    }

    #[test]
    fn workspace_source_falls_back_to_runner_anyhome_addr() {
        let mut a = agent("x");
        a["metadata"]["name"] = json!("ag-9");
        assert_eq!(workspace_source(&a, None), None);
        assert_eq!(
            workspace_source(&a, Some("10.0.0.5:2049")),
            Some(("10.0.0.5:2049".into(), "/agent/ag-9".into()))
        );
        // Annotation wins over the configured fallback.
        a["metadata"]["annotations"] =
            json!({WORKSPACE_ANNOTATION: "nfs://h:1/agent/ag-9"});
        assert_eq!(
            workspace_source(&a, Some("10.0.0.5:2049")),
            Some(("h:1".into(), "/agent/ag-9".into()))
        );
    }

    #[test]
    fn empty_prompt_is_rejected() {
        assert_eq!(validate_prompt(&agent_view(&agent("")).unwrap()), Err("EmptyPrompt"));
        assert_eq!(validate_prompt(&agent_view(&agent("  ")).unwrap()), Err("EmptyPrompt"));
        assert!(validate_prompt(&agent_view(&agent("go")).unwrap()).is_ok());
    }

    #[test]
    fn spawn_plan_maps_args_and_env() {
        let mut a = agent("create the file");
        a["spec"]["model"] = json!("openai/o4");
        a["spec"]["agent"] = json!("build");
        let view = agent_view(&a).unwrap();
        let ws = Path::new("/ws/ag-1");
        let plan = build_spawn_plan(&view, ws, Some(ws.join("prompt.md")), Some("/usr/bin"));
        assert_eq!(plan.workdir, ws);
        assert_eq!(
            plan.args,
            vec![
                "run", "--workdir", "/ws/ag-1", "--model", "openai/o4", "--agent",
                "build", "--prompt-file", "/ws/ag-1/prompt.md", "create the file"
            ]
        );
        assert_eq!(plan.envs[0], ("HOME".into(), "/ws/ag-1".into()));
        assert_eq!(plan.envs[1], ("XDG_DATA_HOME".into(), "/ws/ag-1/mem".into()));
        assert_eq!(plan.envs[2], ("PATH".into(), "/ws/ag-1/tools:/usr/bin".into()));
        // Minimal form: no persona file, no overrides, no inherited PATH.
        let bare = build_spawn_plan(&agent_view(&agent("hi")).unwrap(), ws, None, None);
        assert_eq!(bare.args, vec!["run", "--workdir", "/ws/ag-1", "hi"]);
        assert_eq!(bare.envs[2], ("PATH".into(), "/ws/ag-1/tools".into()));
    }

    #[test]
    fn status_patch_and_condition_shape() {
        let p = status_patch(
            PHASE_RUNNING,
            &[condition(COND_READY, "True", "Running", "runner started")],
        );
        assert_eq!(p["status"]["phase"], "Running");
        assert_eq!(p["status"]["conditions"][0]["type"], "Ready");
        assert_eq!(p["status"]["conditions"][0]["reason"], "Running");
    }

    #[test]
    fn exit_outcome_transitions() {
        assert_eq!(terminal_phase(ExitOutcome::Code(0)), PHASE_SUCCEEDED);
        assert_eq!(terminal_phase(ExitOutcome::Code(3)), PHASE_FAILED);
        assert_eq!(terminal_phase(ExitOutcome::Killed), PHASE_FAILED);
    }

    #[test]
    fn session_id_parses_from_stderr_tail() {
        let raw = "some output\n\n\u{1b}[2m[session ab12cd]\u{1b}[0m\nhint";
        assert_eq!(session_id_from_stderr(raw), Some("ab12cd".into()));
        assert_eq!(session_id_from_stderr("nothing here"), None);
        assert_eq!(session_id_from_stderr("[session ]"), None);
    }
}
