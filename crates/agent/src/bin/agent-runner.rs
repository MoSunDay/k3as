//! T7.7 **D7** (Q63) hardened Agent runner — runs INSIDE the sandbox as
//! `/runner/agent-runner` (uid 1000): it embeds opencoder-session
//! IN-PROCESS and wraps the whole tool registry with the central action
//! gate (`agent::runner_gate`), so every tool dispatch — including
//! hallucinated out-of-schema names and all subagent descendants (the
//! wrapped parent registry map is inherited verbatim) — flows through
//! `api::agent_policy::gate` with a deterministic denial + a secret-free
//! audit line.
//!
//! THE Q36-D1 GUARD: this binary must NEVER be linked into the init-pro
//! multicall binary — opencoder links libsql 0.10.x while init-pro vendors
//! 0.9.x, and an in-process link duplicates C symbols. It is a SEPARATE
//! `[[bin]]` target of the `agent` crate, staged by the node via
//! `runtime::opencoder::stage_agent_runner`.
//!
//! Modes: normal (LLM session, session id = `--attempt`, exit 0/7) and
//! `--probe` (NO LLM, NO session: one gated tool dispatch or a
//! `--probe-list` advertisement dump; exit 0/1/4/5/6).

#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use opencoder_core::{effective_default_agent, resolve_agent, Config, ToolContext, ToolFilter};
use opencoder_llm::{ChatClient, ChatStream};
use opencoder_session::runner::run_with_registry;
use opencoder_session::{tools, SessionState};

use agent::runner_gate::{advertised_tools, wrap_registry, ALL_TOOL_NAMES};
use api::agent_policy::{EffectivePolicy, SANDBOX_AUDIT_FILE, SANDBOX_POLICY_FILE};

/// Marker inside every gate denial (must match `api::agent_policy::gate`).
const DENIED_MARK: &str = "denied by the agent action policy";

#[derive(Debug, Parser)]
#[command(
    name = "agent-runner",
    about = "T7.7 (Q63) hardened in-process Agent runner (D7)"
)]
struct Args {
    /// Working directory (the sandbox `/workspace`).
    #[arg(long)]
    workdir: PathBuf,
    /// Agent name override (default: config default or "act").
    #[arg(long)]
    agent: Option<String>,
    /// Model override (`provider/model`).
    #[arg(long)]
    model: Option<String>,
    /// File whose content replaces the agent system prompt.
    #[arg(long)]
    prompt_file: Option<PathBuf>,
    /// Effective policy file (written by the node; fail-closed).
    #[arg(long, default_value = SANDBOX_POLICY_FILE)]
    policy: PathBuf,
    /// Denial audit JSONL path (append-only, best-effort).
    #[arg(long, default_value = SANDBOX_AUDIT_FILE)]
    audit: PathBuf,
    /// Attempt id (doubles as the session id).
    #[arg(long, default_value = "unknown-attempt")]
    attempt: String,
    /// Probe mode: dispatch ONE tool through the gate (no LLM/session).
    #[arg(long)]
    probe: Option<String>,
    /// JSON input object for `--probe`.
    #[arg(long, default_value = "{}")]
    probe_input: String,
    /// Probe mode: print the advertised tool names and exit.
    #[arg(long)]
    probe_list: bool,
    /// The user prompt (positional).
    prompt: Option<String>,
}

/// Fail-closed policy load: unreadable or malformed => Err (the caller
/// maps it to a nonzero exit, never a silent ungated run).
fn load_policy(path: &std::path::Path) -> Result<EffectivePolicy> {
    let raw = std::fs::read_to_string(path).with_context(|| {
        format!(
            "policy file {}: unreadable (fail-closed, Q63)",
            path.display()
        )
    })?;
    serde_json::from_str(&raw).with_context(|| {
        format!(
            "policy file {}: malformed (fail-closed, Q63)",
            path.display()
        )
    })
}

/// Exit codes (D7 contract; kubelet maps 7 -> RunnerFailed).
const EXIT_RUNNER_FAILED: u8 = 7;
const EXIT_POLICY_INVALID: u8 = 1;
const EXIT_DENIED: u8 = 4;
const EXIT_UNKNOWN_TOOL: u8 = 5;
const EXIT_TOOL_ERROR: u8 = 6;

#[tokio::main]
async fn main() -> ExitCode {
    let args = Args::parse();
    // Fail-closed FIRST in every mode: no policy, no dispatch (Q63).
    let eff = match load_policy(&args.policy) {
        Ok(eff) => eff,
        Err(e) => {
            eprintln!("agent-runner: {e:#}");
            return ExitCode::from(if args.probe.is_some() || args.probe_list {
                EXIT_POLICY_INVALID
            } else {
                EXIT_RUNNER_FAILED
            });
        }
    };
    let agent_name = args.agent.clone().unwrap_or_else(|| "probe".to_string());
    // --probe OR --probe-list => the deterministic no-LLM surface.
    if args.probe.is_some() || args.probe_list {
        probe(args, &eff, &agent_name).await
    } else {
        run_session(args, eff).await
    }
}

/// `--probe` / `--probe-list`: one gated dispatch or the advertisement
/// dump. NO LLM, NO session — deterministic CI/e2e surface.
async fn probe(args: Args, eff: &EffectivePolicy, agent_name: &str) -> ExitCode {
    let base = tools::registry();
    let gated = wrap_registry(base, eff, agent_name, &args.attempt, &args.audit);
    if args.probe_list {
        for name in advertised_tools(&ALL_TOOL_NAMES, eff) {
            println!("{name}");
        }
        return ExitCode::SUCCESS;
    }
    let Some(tool_name) = args.probe.as_deref() else {
        eprintln!("agent-runner: --probe-list requires no --probe");
        return ExitCode::from(EXIT_POLICY_INVALID);
    };
    let Some(tool) = gated.get(tool_name) else {
        eprintln!("agent-runner: unknown tool: {tool_name}");
        return ExitCode::from(EXIT_UNKNOWN_TOOL);
    };
    let input: serde_json::Value = match serde_json::from_str(&args.probe_input) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("agent-runner: --probe-input is not JSON: {e}");
            return ExitCode::from(EXIT_TOOL_ERROR);
        }
    };
    let ctx = ToolContext {
        session_id: args.attempt.clone(),
        message_id: "probe".to_string(),
        agent: agent_name.to_string(),
        working_dir: args.workdir.clone(),
        max_output: 4096,
        proxy: None,
        tools_path: None,
    };
    // Drive the one dispatch directly on the runtime (probe mode never
    // needs more than a single await point).
    let out = match tool.execute(input, &ctx).await {
        Ok(out) => out,
        Err(e) => {
            eprintln!("agent-runner: tool crashed: {e:#}");
            return ExitCode::from(EXIT_TOOL_ERROR);
        }
    };
    if out.is_error {
        println!("{}", out.content);
        if out.content.contains(DENIED_MARK) {
            return ExitCode::from(EXIT_DENIED);
        }
        return ExitCode::from(EXIT_TOOL_ERROR);
    }
    println!("PROBE-OK {}", out.content);
    ExitCode::SUCCESS
}

/// Normal mode: a minimal headless opencode session (mirrors upstream
/// `run_headless`) under the fully-gated registry. Session id = --attempt
/// (keeps the `[session <id>]` stderr contract the kubelet parses).
async fn run_session(args: Args, eff: EffectivePolicy) -> ExitCode {
    match run_session_inner(&args, &eff).await {
        Ok(id) => {
            eprintln!("[session {id}]");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("agent-runner: {e:#}");
            ExitCode::from(EXIT_RUNNER_FAILED)
        }
    }
}

async fn run_session_inner(args: &Args, eff: &EffectivePolicy) -> Result<String> {
    let workdir = &args.workdir;
    let mut config = Config::load(workdir)?;
    // --model override: the simple direct set (upstream apply_model_override
    // pattern; a malformed value still surfaces via resolve_endpoint).
    if let Some(m) = &args.model {
        config.model = m.clone();
    }
    let agent_name = effective_default_agent(args.agent.as_deref(), &config);
    let mut agent =
        resolve_agent(&agent_name).with_context(|| format!("agent not found: {agent_name}"))?;
    // OVERRIDE RESISTANCE (Q63): the advertisement filter is forced AFTER
    // resolution — neither config nor --agent can expand the tool surface.
    agent.tools = ToolFilter::Allow(advertised_tools(&ALL_TOOL_NAMES, eff));
    let ep = config.resolve_endpoint()?;
    let client: Arc<dyn ChatStream> = Arc::new(ChatClient::new_with_read_timeout(
        &ep.base_url,
        &ep.api_key,
        &ep.headers,
        config.stream_idle_timeout(),
        config.network.proxy.as_deref(),
    )?);
    let mut session = SessionState::new(
        args.attempt.clone(),
        agent,
        config.clone(),
        client,
        workdir.clone(),
    );
    if let Some(pf) = &args.prompt_file {
        let body = std::fs::read_to_string(pf)
            .with_context(|| format!("--prompt-file {}: unreadable", pf.display()))?;
        session.agent.prompt = body;
    }
    let prompt = args.prompt.clone().unwrap_or_default();
    let base = tools::registry();
    let gated = wrap_registry(base, eff, &agent_name, &args.attempt, &args.audit);
    run_with_registry(&mut session, prompt, Vec::new(), &gated, |ev| {
        eprintln!("{ev:?}")
    })
    .await?;
    Ok(args.attempt.clone())
}
