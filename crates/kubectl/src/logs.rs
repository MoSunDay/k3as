//! `kubectl logs (POD | pod/POD | pods/POD)` (TODO **T4.2 Scope C**).
//!
//! Two layers, mirroring rollout.rs:
//! - pure helpers ([`parse_logs_args`], [`log_path`], [`next_batch`],
//!   [`advance`], [`status_error_text`] — unit-tested in `logs_tests.rs`);
//! - the verb: one-shot prints the body VERBATIM (`text/plain`, decision
//!   **Q31**); `-f/--follow` polls GET every 250ms (the Q21 cadence) and
//!   locally diffs by LINE COUNT.
//!
//! Wire shape is the frozen T4.2 contract:
//! `GET <server>/api/v1/namespaces/<ns>/pods/<name>/log[?container=C][&tailLines=N]`.

use std::io::Write;
use std::process::ExitCode;
use std::time::Duration;

use serde_json::Value;

use crate::http::HttpClient;

/// Q21 poll cadence for follow mode.
const POLL_INTERVAL: Duration = Duration::from_millis(250);
/// The exact v1 compromise for `-f --tail`: one stderr line; the tail seeds
/// ONLY the initial snapshot, later polls fetch the full log and diff.
pub(crate) const TAIL_FOLLOW_WARNING: &str =
    "warning: --tail is ignored with --follow in init-pro v1";
/// Emitted at most once per follow run when the log shrinks.
pub(crate) const SHRINK_WARNING: &str =
    "warning: log shrank (rotated or truncated); resuming from the end";

/// Parsed `kubectl logs` invocation (pure parse output).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LogsInvocation {
    pub(crate) server: String,
    pub(crate) namespace: String,
    pub(crate) name: String,
    pub(crate) container: Option<String>,
    pub(crate) tail: Option<u64>,
    pub(crate) follow: bool,
}

/// Parse the args after dispatch (`args[0] == "logs"`); unknown flags are
/// hard errors, exactly one positional resource is accepted.
pub(crate) fn parse_logs_args(args: &[String]) -> Result<LogsInvocation, String> {
    let mut server = crate::default_server();
    let mut namespace = "default".to_string();
    let mut container: Option<String> = None;
    let mut tail: Option<u64> = None;
    let mut follow = false;
    let mut positional: Vec<String> = Vec::new();

    let mut idx = 1usize;
    while idx < args.len() {
        let arg = args[idx].as_str();
        let (name, inline): (&str, Option<&str>) = match arg.split_once('=') {
            Some((n, v)) => (n, Some(v)),
            None => (arg, None),
        };
        match name {
            "-f" | "--follow" => follow = true,
            "--server" | "--namespace" | "-n" | "--container" | "-c" | "--tail" => {
                let value: String = match inline {
                    Some(v) => v.to_string(),
                    None => {
                        idx += 1;
                        args.get(idx)
                            .cloned()
                            .ok_or_else(|| format!("flag needs an argument: {name}"))?
                    }
                };
                match name {
                    "--server" => server = value,
                    "--namespace" | "-n" => namespace = value,
                    "--container" | "-c" => container = Some(value),
                    _ => tail = Some(parse_tail(&value)?),
                }
            }
            _ if name.starts_with('-') && name.len() > 1 => {
                return Err(format!("unknown flag: {name}"));
            }
            _ => positional.push(arg.to_string()),
        }
        idx += 1;
    }

    if positional.len() > 1 {
        return Err(format!(
            "unexpected extra arguments: {}",
            positional[1..].join(" ")
        ));
    }
    let name = positional
        .first()
        .ok_or("you must specify exactly one pod: kubectl logs (POD | pod/POD | pods/POD)")?;
    Ok(LogsInvocation {
        server,
        namespace,
        name: resolve_pod_name(name)?,
        container,
        tail,
        follow,
    })
}

/// `--tail` needs a positive integer argument (0/negatives/garbage reject).
fn parse_tail(raw: &str) -> Result<u64, String> {
    match raw.parse::<u64>() {
        Ok(n @ 1..) => Ok(n),
        _ => Err(format!(
            "invalid --tail {raw:?}: must be a positive integer"
        )),
    }
}

/// Target forms: bare NAME, `pod/NAME`, or `pods/NAME`; anything else errors.
pub(crate) fn resolve_pod_name(arg: &str) -> Result<String, String> {
    let name = match arg.split_once('/') {
        Some(("pod", n)) | Some(("pods", n)) | Some(("POD", n)) | Some(("PODS", n)) => {
            n.to_string()
        }
        Some((kind, _)) => {
            return Err(format!(
                "unsupported resource kind {kind:?} for this verb (v1 supports pod | pods)"
            ))
        }
        None => arg.to_string(),
    };
    if name.is_empty() {
        return Err("resource name must not be empty".into());
    }
    Ok(name)
}

/// Frozen-contract request path; `container` / `tailLines` segments appear
/// only when set, in that order.
pub(crate) fn log_path(ns: &str, name: &str, container: Option<&str>, tail: Option<u64>) -> String {
    let mut path = format!("/api/v1/namespaces/{ns}/pods/{name}/log");
    let mut query: Vec<String> = Vec::new();
    if let Some(c) = container {
        query.push(format!("container={c}"));
    }
    if let Some(t) = tail {
        query.push(format!("tailLines={t}"));
    }
    if !query.is_empty() {
        path.push('?');
        path.push_str(&query.join("&"));
    }
    path
}

/// One log GET returning `(status, lowercase content-type, lossy text)`.
pub(crate) async fn fetch_log(
    http: &HttpClient,
    inv: &LogsInvocation,
    tail_seed: Option<u64>,
) -> Result<(u16, String, String), String> {
    let path = log_path(
        &inv.namespace,
        &inv.name,
        inv.container.as_deref(),
        tail_seed,
    );
    http.request_bytes("GET", &path, None, None).await
}

/// Upstream-shaped Status error line:
/// `Error from server (<reason|code>): <message|raw body>`.
pub(crate) fn status_error_text(code: u16, body: &str) -> String {
    let parsed: Option<Value> = serde_json::from_str(body).ok();
    let field = |key: &str| {
        parsed
            .as_ref()
            .and_then(|value| value.get(key))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let reason = field("reason").unwrap_or_else(|| code.to_string());
    let message = field("message").unwrap_or_else(|| body.trim().to_string());
    format!("Error from server ({reason}): {message}")
}

// ------------------------------------------------------------ pure diff core

/// Line count with `str::lines()` semantics (trailing `\n` adds no line).
pub(crate) fn count_lines(text: &str) -> usize {
    text.lines().count()
}

/// True when the new snapshot has FEWER lines than we already printed —
/// rotation/truncation. Callers warn once and reset.
pub(crate) fn log_shrank(prev_seen: usize, full_text: &str) -> bool {
    prev_seen > count_lines(full_text)
}

/// Diff-by-line-count: the unseen lines plus the new counter.
///
/// - growth: lines `[prev_seen..]` joined with one trailing newline,
///   counter -> current len;
/// - steady: `(len, None)` (len == prev_seen);
/// - shrink (rotation): reprint NOTHING, counter resets to `len`.
pub(crate) fn next_batch(prev_seen: usize, full_text: &str) -> (usize, Option<String>) {
    let lines: Vec<&str> = full_text.lines().collect();
    if prev_seen < lines.len() {
        let mut batch = lines[prev_seen..].join("\n");
        batch.push('\n');
        (lines.len(), Some(batch))
    } else {
        // Steady OR shrunk-below-watermark: never re-print old lines.
        (lines.len(), None)
    }
}

/// Follow-loop state (kept outside the loop so tests drive it purely).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FollowState {
    pub(crate) seen: usize,
    pub(crate) warned_shrink: bool,
}

/// One pure follow step over a fetched snapshot: `(next state, stdout,
/// stderr)` — warnings carry through stderr at most once per shrink event.
pub(crate) fn advance(state: &FollowState, full_text: &str) -> (FollowState, String, String) {
    let shrunk = log_shrank(state.seen, full_text);
    let (seen, batch) = next_batch(state.seen, full_text);
    let mut stderr = String::new();
    if shrunk && !state.warned_shrink {
        stderr.push_str(SHRINK_WARNING);
        stderr.push('\n');
    }
    (
        FollowState {
            seen,
            warned_shrink: state.warned_shrink || shrunk,
        },
        batch.unwrap_or_default(),
        stderr,
    )
}

// ----------------------------------------------------------------- the verb

fn exit_usage_error(msg: &str) -> ExitCode {
    eprintln!("error: {msg}");
    eprintln!();
    eprintln!("Usage: kubectl logs (POD | pod/POD | pods/POD) [-f|--follow] [--tail=N] [-c CONTAINER] [flags]");
    ExitCode::from(1)
}

/// Entry from the multicall dispatcher: `Some("logs") => logs::run(args)`.
pub(crate) fn run(args: &[String]) -> ExitCode {
    let inv = match parse_logs_args(args) {
        Ok(inv) => inv,
        Err(msg) => return exit_usage_error(&msg),
    };
    // The exact v1 `-f --tail` compromise, warned exactly once up-front.
    if inv.follow && inv.tail.is_some() {
        eprintln!("{TAIL_FOLLOW_WARNING}");
    }
    let http = match HttpClient::parse(&inv.server) {
        Ok(http) => http,
        Err(msg) => return crate::verb_error(msg),
    };
    let rt = match crate::runtime() {
        Ok(rt) => rt,
        Err(msg) => return crate::verb_error(msg),
    };
    rt.block_on(async move {
        if inv.follow {
            follow(&http, &inv).await
        } else {
            once(&http, &inv).await
        }
    })
}

/// One-shot: print the raw body VERBATIM (no extra newline, Q31).
async fn once(http: &HttpClient, inv: &LogsInvocation) -> ExitCode {
    match fetch_log(http, inv, inv.tail).await {
        Ok((code, _, body)) if (200..300).contains(&code) => {
            print!("{body}");
            let _ = std::io::stdout().flush();
            ExitCode::SUCCESS
        }
        Ok((code, _, body)) => {
            eprintln!("{}", status_error_text(code, &body));
            ExitCode::from(1)
        }
        Err(msg) => {
            eprintln!("error: {msg}");
            ExitCode::from(1)
        }
    }
}

/// Follow mode: 250ms polls until Ctrl-C. FIRST poll seeds with `--tail`
/// when set; later polls take the FULL log and diff by line count.
async fn follow(http: &HttpClient, inv: &LogsInvocation) -> ExitCode {
    let mut seeded = false;
    let mut state = FollowState {
        seen: 0,
        warned_shrink: false,
    };
    loop {
        let tail_seed = if seeded { None } else { inv.tail };
        seeded = true;
        match fetch_log(http, inv, tail_seed).await {
            Ok((code, _, body)) if (200..300).contains(&code) => {
                let (next, stdout_line, stderr_line) = advance(&state, &body);
                state = next;
                print!("{stdout_line}");
                let _ = std::io::stdout().flush();
                eprint!("{stderr_line}");
            }
            Ok((code, _, body)) => {
                eprintln!("{}", status_error_text(code, &body));
                return ExitCode::from(1);
            }
            Err(msg) => {
                eprintln!("error: {msg}");
                return ExitCode::from(1);
            }
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}
