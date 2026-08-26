//! `kubectl exec POD -- CMD [ARGS...]` (TODO **T4.2 Scope C**, Q31).
//!
//! Pure grammar helpers ([`parse_exec_args`], [`exit_code_of`],
//! [`exec_path`]) are unit-tested inline; wire-level cases live in
//! `exec_wire_tests.rs`. The frozen-contract request shape is
//! `POST <server>/api/v1/namespaces/<ns>/pods/<name>/exec[?container=C]`
//! with a JSON `{"command":[...]}` body; the relay answers
//! `{"exitCode":<int>,"output":"<str>"}`.

use std::io::Write;
use std::process::ExitCode;

use serde_json::{json, Value};

use crate::http::HttpClient;

/// Upstream-parity message for a missing literal `--` command tail.
pub(crate) const MISSING_COMMAND: &str = "you must specify at least one command for the container";

/// Parsed `kubectl exec` invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExecInvocation {
    pub(crate) server: String,
    pub(crate) namespace: String,
    pub(crate) name: String,
    pub(crate) container: Option<String>,
    pub(crate) command: Vec<String>,
}

/// Parse the args after dispatch. Everything before the LITERAL `--` is
/// kubectl grammar; everything after it is the remote argv verbatim
/// (including further `-x`-looking tokens). One positional pod required.
pub(crate) fn parse_exec_args(args: &[String]) -> Result<ExecInvocation, String> {
    let dashdash = args.iter().position(|a| a == "--");
    let Some(dash_pos) = dashdash else {
        return Err(MISSING_COMMAND.to_string());
    };
    let command: Vec<String> = args[dash_pos + 1..].to_vec();
    if command.is_empty() {
        return Err(MISSING_COMMAND.to_string());
    }

    let mut server = crate::default_server();
    let mut namespace = "default".to_string();
    let mut container: Option<String> = None;
    let mut positional: Vec<String> = Vec::new();

    let mut idx = 1usize; // args[0] is the verb word itself
    while idx < dash_pos {
        let arg = args[idx].as_str();
        let (name, inline): (&str, Option<&str>) = match arg.split_once('=') {
            Some((n, v)) => (n, Some(v)),
            None => (arg, None),
        };
        match name {
            "--server" | "--namespace" | "-n" | "--container" | "-c" => {
                let value: String = match inline {
                    Some(v) => v.to_string(),
                    None => {
                        idx += 1;
                        if idx >= dash_pos {
                            return Err(format!("flag needs an argument: {name}"));
                        }
                        args[idx].clone()
                    }
                };
                match name {
                    "--server" => server = value,
                    "--namespace" | "-n" => namespace = value,
                    _ => container = Some(value),
                }
            }
            _ if name.starts_with('-') && name.len() > 1 => {
                return Err(format!("unknown flag: {name}"))
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
    let name = positional.first().ok_or("pod name is required")?;
    Ok(ExecInvocation {
        server,
        namespace,
        name: crate::logs::resolve_pod_name(name)?,
        container,
        command,
    })
}

/// Frozen-contract request path; the `container` selector is optional.
pub(crate) fn exec_path(ns: &str, name: &str, container: Option<&str>) -> String {
    match container {
        Some(c) => format!("/api/v1/namespaces/{ns}/pods/{name}/exec?container={c}"),
        None => format!("/api/v1/namespaces/{ns}/pods/{name}/exec"),
    }
}

/// Map a remote exit status into this process's exit code byte:
/// negative (killed/unknown) -> 1, values over 255 clamp to 255.
pub(crate) fn exit_code_of(rc: i64) -> u8 {
    if rc < 0 {
        1
    } else {
        u8::try_from(rc).unwrap_or(255)
    }
}

/// Status-shaped stderr line for non-2xx relay replies (`.message` wins).
pub(crate) fn status_error_text(code: u16, body: &Value) -> String {
    let reason = body
        .get("reason")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| code.to_string());
    let message = body
        .get("message")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| body.to_string());
    format!("Error from server ({reason}): {message}")
}

/// POST the exec request and unpack the relay reply into
/// `(exit_code, stdout_output)`; non-2xx replies become
/// `Err("Error from server (<reason|code>): <message>")`.
pub(crate) async fn pod_exec(
    http: &HttpClient,
    inv: &ExecInvocation,
) -> Result<(i64, String), String> {
    let path = exec_path(&inv.namespace, &inv.name, inv.container.as_deref());
    let payload = serde_json::to_string(&json!({ "command": inv.command }))
        .map_err(|e| format!("encode exec command failed: {e}"))?;
    let (code, body) = http
        .request_json("POST", &path, Some("application/json"), Some(&payload))
        .await?;
    if !(200..300).contains(&code) {
        return Err(status_error_text(code, &body));
    }
    // Missing/mistyped fields on a 200 mean the reply contract broke.
    let rc = body
        .get("exitCode")
        .and_then(Value::as_i64)
        .ok_or("malformed exec response: missing numeric exitCode")?;
    let output = body
        .get("output")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    Ok((rc, output))
}

fn exit_usage_error(msg: &str) -> ExitCode {
    eprintln!("error: {msg}");
    eprintln!();
    eprintln!("Usage: kubectl exec POD [-n NAMESPACE] [-c CONTAINER] -- COMMAND [args...]");
    ExitCode::from(1)
}

/// Entry from the multicall dispatcher: `Some("exec") => exec_cmd::run`.
pub(crate) fn run(args: &[String]) -> ExitCode {
    let inv = match parse_exec_args(args) {
        Ok(inv) => inv,
        Err(msg) => return exit_usage_error(&msg),
    };
    let http = match HttpClient::parse(&inv.server) {
        Ok(http) => http,
        Err(msg) => return crate::verb_error(msg),
    };
    let rt = match crate::runtime() {
        Ok(rt) => rt,
        Err(msg) => return crate::verb_error(msg),
    };
    rt.block_on(async move {
        match pod_exec(&http, &inv).await {
            Ok((rc, output)) => {
                print!("{output}");
                let _ = std::io::stdout().flush();
                if rc == 0 {
                    ExitCode::SUCCESS
                } else {
                    ExitCode::from(exit_code_of(rc))
                }
            }
            Err(msg) => crate::verb_error(msg),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn exit_code_mapping_clamps_and_defaults() {
        assert_eq!(exit_code_of(0), 0);
        assert_eq!(exit_code_of(1), 1);
        assert_eq!(exit_code_of(42), 42);
        assert_eq!(exit_code_of(-1), 1, "negative (signal/unknown) maps to 1");
        assert_eq!(exit_code_of(-15), 1);
        assert_eq!(exit_code_of(256), 255, "over-u8 clamps to 255");
        assert_eq!(exit_code_of(i64::MAX), 255);
    }

    #[test]
    fn parse_requires_literal_dashdash_and_a_command() {
        let err = parse_exec_args(&args(&["exec", "web", "ls", "/"])).unwrap_err();
        assert_eq!(err, MISSING_COMMAND);
        let err = parse_exec_args(&args(&["exec", "web", "--"])).unwrap_err();
        assert_eq!(err, MISSING_COMMAND);

        let inv = parse_exec_args(&args(&[
            "exec", "web", "-c", "app", "--", "sh", "-c", "echo hi",
        ]))
        .unwrap();
        assert_eq!(inv.name, "web");
        assert_eq!(inv.container.as_deref(), Some("app"));
        assert_eq!(inv.command, vec!["sh", "-c", "echo hi"]);

        // Tokens after `--` keep their dashes verbatim.
        let inv = parse_exec_args(&args(&["exec", "pods/web", "--", "-flag", "--x=y"])).unwrap();
        assert_eq!(inv.command, vec!["-flag", "--x=y"]);
        assert!(parse_exec_args(&args(&["exec", "a", "b", "--", "ls"])).is_err());
    }

    #[test]
    fn exec_paths_match_the_frozen_contract() {
        assert_eq!(
            exec_path("default", "web", None),
            "/api/v1/namespaces/default/pods/web/exec"
        );
        assert_eq!(
            exec_path("ns", "w", Some("-")),
            "/api/v1/namespaces/ns/pods/w/exec?container=-"
        );
    }

    #[test]
    fn status_errors_extract_message() {
        let body: Value =
            serde_json::from_str(r#"{"reason":"NotFound","message":"pods \"x\" not found"}"#)
                .unwrap();
        assert_eq!(
            status_error_text(404, &body),
            "Error from server (NotFound): pods \"x\" not found"
        );
        assert_eq!(
            status_error_text(502, &Value::Null),
            "Error from server (502): null"
        );
    }
}
