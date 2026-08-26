//! `kubectl` — the first real kubectl surface inside init-pro (T3.1b).
//!
//! Invoked via the multicall `argv[0]` dispatch (`ln -s init-pro kubectl`);
//! the CLI clap path is intentionally untouched (plan T3.1b: argv[0] only).
//!
//! v1 surface: `kubectl rollout status (deployment/NAME | deployment NAME)`
//! plus the argoproj.io verbs (T7.3 Scope A, Q30). Simplifications are
//! locked by decision **Q21**:
//! - transport: plain HTTP/1.1 over a raw `tokio::TcpStream`, no auth, no
//!   https, no new dependencies (TLS + auth arrive with T1.3);
//! - waiting: poll GET every 250ms instead of a watch stream.

#![forbid(unsafe_code)]

mod discovery;
#[cfg(test)]
mod discovery_tests;
#[cfg(test)] // canned-TCP-server cases (file size limit, mirrors verbs_wire_tests)
mod discovery_wire_tests;
mod exec_cmd;
#[cfg(test)] // canned-apiserver cases for the exec verb (file size limit)
mod exec_wire_tests;
mod http;
mod logs;
#[cfg(test)] // pure follow/parser cases split out of logs.rs (file size limit)
mod logs_tests;
mod rollout;
#[cfg(test)]
mod rollout_poll_tests;
mod verbs;
#[cfg(test)]
mod verbs_tests;
#[cfg(test)] // wire-level cases split out of verbs_tests (file size limit)
mod verbs_wire_tests;

use std::process::ExitCode;
use std::time::Duration;

use discovery::{api_resources, api_versions};
use http::HttpClient;
use rollout::{rollout_status, Outcome};
use verbs::{apply, delete, get, parse_output, resolve_plural, Output};

/// Default apiserver address (server default bind is 127.0.0.1:6443).
pub(crate) const DEFAULT_SERVER: &str = "http://127.0.0.1:6443";
/// Default namespace, kubectl parity.
pub(crate) const DEFAULT_NAMESPACE: &str = "default";

/// Pure env-fallback resolution (T4.2 Scope C): a non-empty `env` value
/// beats [`DEFAULT_SERVER`], and an explicit `--server` flag in turn beats
/// the env (flags overwrite the initialized default). Kept pure so tests
/// cover precedence without process-global state.
fn default_server_from(env: Option<String>) -> String {
    match env {
        Some(v) if !v.trim().is_empty() => v,
        _ => DEFAULT_SERVER.to_string(),
    }
}

/// [`DEFAULT_SERVER`] unless `INIT_PRO_SERVER` overrides it.
fn default_server() -> String {
    default_server_from(std::env::var("INIT_PRO_SERVER").ok())
}

/// Parsed `kubectl rollout status` invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Config {
    server: String,
    namespace: String,
    name: String,
    timeout: Option<Duration>,
}

/// Entry point from the multicall dispatcher (`argv[0] == kubectl`).
pub fn run(args: &[String]) -> ExitCode {
    // Help anywhere wins (scripts/multicall-selftest.sh relies on
    // `kubectl --help` exiting 0).
    if args
        .iter()
        .any(|a| matches!(a.as_str(), "-h" | "--help" | "help"))
    {
        print!("{}", usage());
        return ExitCode::SUCCESS;
    }

    // T7.3 Scope A (Q30): argoproj.io verbs dispatch first; everything
    // else keeps flowing into the Q21 rollout parser below.
    match args.first().map(String::as_str) {
        Some("get") => return run_get(args),
        Some("apply") => return run_apply(args),
        Some("delete") => return run_delete(args),
        // T4.2 Scope C (Q31): pod log + exec over the apiserver relay.
        Some("logs") => return crate::logs::run(args),
        Some("exec") => return crate::exec_cmd::run(args),
        // T7.3 optional enhancement (Q30): discovery listing verbs.
        Some("api-versions") => return run_discovery(args, "api-versions"),
        Some("api-resources") => return run_discovery(args, "api-resources"),
        _ => {}
    }

    let config = match parse_args(args) {
        Ok(c) => c,
        Err(msg) => {
            eprintln!("error: {msg}");
            eprintln!();
            eprintln!("To run this command, type \"kubectl rollout status <deployment>\"");
            return ExitCode::from(1);
        }
    };
    let http = match HttpClient::parse(&config.server) {
        Ok(h) => h,
        Err(msg) => {
            eprintln!("error: {msg}");
            return ExitCode::from(1);
        }
    };

    // Q21: a tiny current-thread runtime is all the poll loop needs.
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("error: failed to start tokio runtime: {e}");
            return ExitCode::from(1);
        }
    };
    let outcome = rt.block_on(rollout_status(
        &http,
        &config.namespace,
        &config.name,
        config.timeout,
    ));
    match outcome {
        // rollout_status already printed the success line to stdout.
        Outcome::Success(_) => ExitCode::SUCCESS,
        Outcome::Failure(msg) | Outcome::Timeout(msg) => {
            eprintln!("{msg}");
            ExitCode::from(1)
        }
    }
}

/// Parsed flags shared by the argoproj.io verbs (T7.3 Scope A).
#[derive(Debug, Clone, PartialEq, Eq)]
struct VerbFlags {
    server: String,
    namespace: String,
    output: Output,
    /// `apply -f FILE`.
    file: Option<String>,
}

/// Parse the flags after the verb word (`--server`, `-n|--namespace`,
/// `-o|--output` when `allow_output`, `-f|--filename` when `allow_file`)
/// plus the bare positionals. `--flag value` and `--flag=value` both work;
/// anything else starting with `-` is an unknown flag (rollout-style error).
fn parse_verb_flags(
    args: &[String],
    allow_output: bool,
    allow_file: bool,
) -> Result<(VerbFlags, Vec<String>), String> {
    let mut flags = VerbFlags {
        server: default_server(),
        namespace: DEFAULT_NAMESPACE.to_string(),
        output: Output::Table,
        file: None,
    };
    let mut positional: Vec<String> = Vec::new();

    let mut idx = 1usize; // args[0] is the verb word itself
    while idx < args.len() {
        let arg = args[idx].as_str();
        let (name, inline): (&str, Option<&str>) = match arg.split_once('=') {
            Some((n, v)) => (n, Some(v)),
            None => (arg, None),
        };
        let is_flag = matches!(
            name,
            "--server" | "--namespace" | "-n" | "-o" | "--output" | "-f" | "--filename"
        );
        if !is_flag {
            if name.starts_with('-') && name.len() > 1 {
                return Err(format!("unknown flag: {name}"));
            }
            positional.push(arg.to_string());
            idx += 1;
            continue;
        }
        if !allow_output && matches!(name, "-o" | "--output") {
            return Err(format!(
                "flag {name} is only supported by \"kubectl get\" in v1"
            ));
        }
        if !allow_file && matches!(name, "-f" | "--filename") {
            return Err(format!(
                "flag {name} is only supported by \"kubectl apply\" in v1"
            ));
        }
        let mut value = inline.map(str::to_string);
        if value.is_none() {
            idx += 1;
            value = args.get(idx).cloned();
        }
        let value = value.ok_or_else(|| format!("flag needs an argument: {name}"))?;
        match name {
            "--server" => flags.server = value,
            "--namespace" | "-n" => flags.namespace = value,
            "-o" | "--output" => flags.output = parse_output(&value)?,
            "-f" | "--filename" => flags.file = Some(value),
            _ => unreachable!("flag set is checked by is_flag above"),
        }
        idx += 1;
    }
    Ok((flags, positional))
}

/// Split positionals into `(plural, Option<name>)`; `TYPE/NAME` or `TYPE
/// NAME`. `required_name` distinguishes `delete` (name mandatory) from
/// `get` (list allowed).
fn resolve_positionals(
    positional: &[String],
    required_name: bool,
) -> Result<(&'static str, Option<String>), String> {
    let (kind, name): (&str, Option<String>) = match positional {
        [single] => match single.split_once('/') {
            Some((k, n)) => (k, Some(n.to_string())),
            None if required_name => {
                return Err("resource must be specified as TYPE/NAME or TYPE NAME".into())
            }
            None => (single.as_str(), None),
        },
        [kind, name] => (kind.as_str(), Some(name.clone())),
        [] => return Err("you must specify a resource type (workflow | cronworkflow)".into()),
        _ => {
            return Err(format!(
                "unexpected extra arguments: {}",
                positional[2..].join(" ")
            ))
        }
    };
    let plural = resolve_plural(kind)?;
    match name.as_deref() {
        Some("") => Err("resource name must not be empty".to_string()),
        other => Ok((plural, other.map(str::to_string))),
    }
}

/// `kubectl get (TYPE | TYPE/NAME | TYPE NAME) [-o json|yaml|name]`.
fn run_get(args: &[String]) -> ExitCode {
    let (flags, plural, name) =
        match parse_verb_flags(args, true, false).and_then(|(flags, positional)| {
            let (plural, name) = resolve_positionals(&positional, false)?;
            Ok((flags, plural, name))
        }) {
            Ok(v) => v,
            Err(msg) => return verb_error(msg),
        };
    let http = match HttpClient::parse(&flags.server) {
        Ok(h) => h,
        Err(msg) => return verb_error(msg),
    };
    let rt = match runtime() {
        Ok(rt) => rt,
        Err(msg) => return verb_error(msg),
    };
    match rt.block_on(get(
        &http,
        &flags.namespace,
        plural,
        name.as_deref(),
        flags.output,
    )) {
        Ok(text) => {
            print!("{text}");
            ExitCode::SUCCESS
        }
        Err(msg) => verb_error(msg),
    }
}

/// `kubectl apply -f FILE` (Server-Side Apply, T7.3 Scope A).
fn run_apply(args: &[String]) -> ExitCode {
    let (flags, file) =
        match parse_verb_flags(args, false, true) {
            Ok((flags, positional)) => match (flags.file.clone(), positional.is_empty()) {
                (_, false) => {
                    return verb_error(format!(
                        "unexpected extra arguments: {}",
                        positional.join(" ")
                    ))
                }
                (Some(file), true) => (flags, file),
                (None, true) => return verb_error(
                    "you must specify --filename (-f) to apply, e.g. \"kubectl apply -f wf.yaml\""
                        .to_string(),
                ),
            },
            Err(msg) => return verb_error(msg),
        };
    let http = match HttpClient::parse(&flags.server) {
        Ok(h) => h,
        Err(msg) => return verb_error(msg),
    };
    let rt = match runtime() {
        Ok(rt) => rt,
        Err(msg) => return verb_error(msg),
    };
    match rt.block_on(apply(&http, &flags.namespace, &file)) {
        Ok(text) => {
            println!("{text}");
            ExitCode::SUCCESS
        }
        Err(msg) => verb_error(msg),
    }
}

/// `kubectl delete (TYPE/NAME | TYPE NAME)`.
fn run_delete(args: &[String]) -> ExitCode {
    let (flags, plural, name) =
        match parse_verb_flags(args, false, false).and_then(|(flags, positional)| {
            let (plural, name) = resolve_positionals(&positional, true)?;
            Ok((flags, plural, name))
        }) {
            Ok(v) => v,
            Err(msg) => return verb_error(msg),
        };
    let Some(name) = name else {
        return verb_error("resource must be specified as TYPE/NAME or TYPE NAME".to_string());
    };
    let http = match HttpClient::parse(&flags.server) {
        Ok(h) => h,
        Err(msg) => return verb_error(msg),
    };
    let rt = match runtime() {
        Ok(rt) => rt,
        Err(msg) => return verb_error(msg),
    };
    match rt.block_on(delete(&http, &flags.namespace, plural, &name)) {
        Ok(text) => {
            println!("{text}");
            ExitCode::SUCCESS
        }
        Err(msg) => verb_error(msg),
    }
}

/// `kubectl api-versions` / `kubectl api-resources` (T7.3 optional
/// enhancement, Q30). Neither takes positional args; only the shared
/// flags (`--server`, `-n`) apply, so both share one thin runner.
fn run_discovery(args: &[String], verb: &str) -> ExitCode {
    let flags = match parse_verb_flags(args, false, false) {
        Ok((flags, positional)) if positional.is_empty() => flags,
        Ok((_, positional)) => {
            return verb_error(format!(
                "unexpected arguments: {} (\"kubectl {verb}\" takes no positional arguments)",
                positional.join(" ")
            ))
        }
        Err(msg) => return verb_error(msg),
    };
    let http = match HttpClient::parse(&flags.server) {
        Ok(h) => h,
        Err(msg) => return verb_error(msg),
    };
    let rt = match runtime() {
        Ok(rt) => rt,
        Err(msg) => return verb_error(msg),
    };
    let outcome = match verb {
        "api-resources" => rt.block_on(api_resources(&http)),
        _ => rt.block_on(api_versions(&http)),
    };
    match outcome {
        Ok(text) => {
            print!("{text}");
            ExitCode::SUCCESS
        }
        Err(msg) => verb_error(msg),
    }
}

/// Uniform failure exit for the T7.3 verbs and the T4.2 logs/exec verbs.
pub(crate) fn verb_error(msg: String) -> ExitCode {
    eprintln!("error: {msg}");
    ExitCode::from(1)
}

/// Q21: a tiny current-thread runtime is all the verbs need.
pub(crate) fn runtime() -> Result<tokio::runtime::Runtime, String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("failed to start tokio runtime: {e}"))
}

/// Parse `rollout status <target> [flags]` into a [`Config`] (pure).
fn parse_args(args: &[String]) -> Result<Config, String> {
    let mut iter = args.iter();
    let Some(sub) = iter.next() else {
        return Err("you must specify a command: \"rollout status\" is the v1 surface".into());
    };
    if sub != "rollout" {
        return Err(format!("unknown command {sub:?} for \"kubectl\""));
    }
    let Some(verb) = iter.next() else {
        return Err("missing subcommand for \"kubectl rollout\" (v1 supports \"status\")".into());
    };
    if verb != "status" {
        return Err(format!(
            "unknown subcommand {verb:?} for \"kubectl rollout\""
        ));
    }

    let mut server = default_server();
    let mut namespace = DEFAULT_NAMESPACE.to_string();
    let mut timeout: Option<Duration> = None;
    let mut positional: Vec<&str> = Vec::new();

    let mut idx = 2usize;
    while idx < args.len() {
        let arg = args[idx].as_str();
        // Value-taking flags: `--flag value` (and `--flag=value` forms).
        let value: Option<&str> = match arg {
            "--server" | "--namespace" | "-n" | "--timeout" => {
                idx += 1;
                args.get(idx).map(String::as_str)
            }
            _ => None,
        };
        match arg {
            "--server" => server = value.ok_or("flag needs an argument: --server")?.to_string(),
            "--namespace" | "-n" => {
                namespace = value
                    .ok_or("flag needs an argument: --namespace")?
                    .to_string()
            }
            "--timeout" => {
                let raw = value.ok_or("flag needs an argument: --timeout")?;
                timeout = Some(parse_timeout(raw)?);
            }
            _ if arg.starts_with("--server=") => server = arg["--server=".len()..].to_string(),
            _ if arg.starts_with("--namespace=") => {
                namespace = arg["--namespace=".len()..].to_string()
            }
            _ if arg.starts_with("--timeout=") => {
                timeout = Some(parse_timeout(&arg["--timeout=".len()..])?)
            }
            _ if arg.starts_with('-') && arg.len() > 1 => {
                return Err(format!("unknown flag: {arg}"));
            }
            _ => positional.push(arg),
        }
        idx += 1;
    }

    let name = resolve_target(&positional)?;
    Ok(Config {
        server,
        namespace,
        name,
        timeout,
    })
}

/// Deployment target: `TYPE/NAME`, or two positionals `TYPE NAME`.
/// v1 accepts only the deployment aliases (T3.1b); anything else errors.
fn resolve_target(positional: &[&str]) -> Result<String, String> {
    let (kind, name): (&str, String) = match positional {
        [single] => {
            let (res, name) = single
                .split_once('/')
                .ok_or("resource must be specified as TYPE/NAME or TYPE NAME")?;
            (res, name.to_string())
        }
        [res, name] => (*res, name.to_string()),
        [] => return Err("resource must be specified as TYPE/NAME or TYPE NAME".into()),
        _ => {
            return Err(format!(
                "unexpected extra arguments: {}",
                positional[2..].join(" ")
            ))
        }
    };
    if !matches!(
        kind.to_ascii_lowercase().as_str(),
        "deployment" | "deployments" | "deploy"
    ) {
        return Err(format!(
            "unsupported resource kind {kind:?} for \"rollout status\" (v1 supports deployment only, T3.1b)"
        ));
    }
    if name.is_empty() {
        return Err("resource name must not be empty".into());
    }
    Ok(name)
}

/// Parse a `--timeout` value: `30s`, `5m`, `1h`, bare seconds (`45`),
/// `0`/`0s` = single attempt. Absent flag = wait forever (None).
fn parse_timeout(raw: &str) -> Result<Duration, String> {
    let raw = raw.trim();
    let (digits, unit_secs) = match raw.chars().last() {
        Some('s') => (&raw[..raw.len() - 1], 1u64),
        Some('m') => (&raw[..raw.len() - 1], 60),
        Some('h') => (&raw[..raw.len() - 1], 3600),
        _ => (raw, 1),
    };
    let count: u64 = digits.parse().map_err(|_| {
        format!("invalid timeout value {raw:?} (use e.g. 30s, 5m, or bare seconds)")
    })?;
    Ok(Duration::from_secs(count * unit_secs))
}

/// Usage text (kubectl-style), printed on `-h/--help/help`.
fn usage() -> String {
    let lines: Vec<String> = vec![
        "kubectl controls the init-pro cluster.".into(),
        String::new(),
        "v1 surface: rollout status for Deployments (T3.1b, Q21 plain-HTTP transport)".into(),
        "plus argoproj.io/v1alpha1 get/apply/delete (T7.3 Scope A, Q30)".into(),
        "the discovery listing verbs api-versions / api-resources (T7.3, Q30),".into(),
        "and pod logs / exec over the apiserver relay (T4.2 Scope C, Q31).".into(),
        String::new(),
        "Usage:".into(),
        "  kubectl rollout status (TYPE/NAME | TYPE NAME) [flags]".into(),
        "  kubectl get (TYPE | TYPE/NAME | TYPE NAME) [-o json|yaml|name] [flags]".into(),
        "  kubectl apply -f FILE [flags]                    (Server-Side Apply, one manifest)".into(),
        "  kubectl delete (TYPE/NAME | TYPE NAME) [flags]".into(),
        "  kubectl api-versions [flags]                     (all served group/version, sorted)".into(),
        "  kubectl api-resources [flags]                    (NAME SHORTNAMES APIVERSION NAMESPACED KIND)".into(),
        "  kubectl logs (POD | pod/POD | pods/POD) [-f] [--tail=N] [-c C] [flags]".into(),
        "  kubectl exec POD [-c CONTAINER] -- CMD [ARGS...] [flags]".into(),
        String::new(),
        "Target forms: deployment/NAME, deployments/NAME, deploy/NAME (deployment only in v1).".into(),
        "argoproj types: workflow (wf), cronworkflow (cwf, cronwf) — argoproj.io/v1alpha1 only.".into(),
        String::new(),
        "Flags:".into(),
        "  -h, --help                 help for kubectl".into(),
        format!(
            "      --server string       http address of the apiserver (default \"{DEFAULT_SERVER}\")"
        ),
        format!("  -n, --namespace string     namespace scope (default \"{DEFAULT_NAMESPACE}\")"),
        "      --timeout duration    max wait, e.g. 30s, 5m, or bare seconds; 0 = poll once".into(),
        "                            (default: wait forever)".into(),
        "  -o, --output string        output format: json|yaml|name (get only, default table)".into(),
        "  -f, --filename string      manifest file to apply (apply only, required)".into(),
    ];
    let mut text = lines.join("\n");
    text.push('\n');
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn parse(list: &[&str]) -> Result<Config, String> {
        parse_args(&args(list))
    }

    #[test]
    fn parse_accepts_all_deployment_aliases() {
        for target in ["deployment/r", "deployments/r", "DEPLOY/r", "Deployment/r"] {
            let c =
                parse(&["rollout", "status", target]).unwrap_or_else(|e| panic!("{target}: {e}"));
            assert_eq!(c.name, "r");
            assert_eq!(c.namespace, "default");
            assert_eq!(c.server, DEFAULT_SERVER);
            assert_eq!(c.timeout, None);
        }
    }

    #[test]
    fn parse_accepts_two_arg_form_and_flags() {
        let c = parse(&[
            "rollout",
            "status",
            "deployment",
            "my-dep",
            "-n",
            "prod",
            "--server",
            "http://10.0.0.1:6443",
            "--timeout",
            "5m",
        ])
        .unwrap();
        assert_eq!(c.name, "my-dep");
        assert_eq!(c.namespace, "prod");
        assert_eq!(c.server, "http://10.0.0.1:6443");
        assert_eq!(c.timeout, Some(Duration::from_secs(300)));
    }

    #[test]
    fn parse_accepts_equals_flag_forms() {
        let c = parse(&[
            "rollout",
            "status",
            "deploy/x",
            "--namespace=kube-system",
            "--server=127.0.0.1:8443",
            "--timeout=30s",
        ])
        .unwrap();
        assert_eq!(c.name, "x");
        assert_eq!(c.namespace, "kube-system");
        assert_eq!(c.server, "127.0.0.1:8443");
        assert_eq!(c.timeout, Some(Duration::from_secs(30)));
    }

    #[test]
    fn parse_rejects_unsupported_kinds() {
        let err = parse(&["rollout", "status", "statefulset/web"]).unwrap_err();
        assert!(err.contains("unsupported resource kind"), "got: {err}");
        assert!(parse(&["rollout", "status", "pod/x"]).is_err());
        assert!(parse(&["rollout", "status", "rs", "x"]).is_err());
    }

    #[test]
    fn parse_rejects_bad_invocations() {
        assert!(parse(&["rollout", "status"]).is_err());
        assert!(parse(&["rollout", "status", "deployment/"]).is_err());
        assert!(parse(&["rollout", "status", "deployment/r", "extra"]).is_err());
        assert!(parse(&["rollout", "restart", "deployment/r"]).is_err());
        assert!(parse(&["get", "pods"]).is_err());
        assert!(parse(&[]).is_err());
        assert!(parse(&["rollout", "status", "deployment/r", "--wat"]).is_err());
        assert!(parse(&["rollout", "status", "deployment/r", "--server"]).is_err());
    }

    #[test]
    fn parse_timeout_accepts_required_forms() {
        assert_eq!(parse_timeout("30s").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_timeout("5m").unwrap(), Duration::from_secs(300));
        assert_eq!(parse_timeout("45").unwrap(), Duration::from_secs(45));
        assert_eq!(parse_timeout("0").unwrap(), Duration::ZERO);
        assert_eq!(parse_timeout("0s").unwrap(), Duration::ZERO);
        assert_eq!(parse_timeout("1h").unwrap(), Duration::from_secs(3600));
        assert!(parse_timeout("abc").is_err());
        assert!(parse_timeout("5x").is_err());
        assert!(parse_timeout("").is_err());
    }

    #[test]
    fn usage_names_the_v1_surface() {
        let text = usage();
        assert!(text.contains("rollout status"));
        assert!(text.contains("--server"));
        assert!(text.contains("--timeout"));
        // T4.2 Scope C verbs are documented too.
        assert!(text.contains("kubectl logs"), "usage must mention logs");
        assert!(text.contains("kubectl exec"), "usage must mention exec");
    }

    #[test]
    fn env_fallback_precedence_is_pure() {
        use crate::default_server_from as from;
        // No env -> canonical default; empty/blank env counts as unset.
        assert_eq!(from(None), DEFAULT_SERVER);
        assert_eq!(from(Some(String::new())), DEFAULT_SERVER);
        assert_eq!(from(Some("   ".to_string())), DEFAULT_SERVER);
        // Non-empty env wins over the default...
        assert_eq!(
            from(Some("http://10.0.0.9:6443".to_string())),
            "http://10.0.0.9:6443"
        );
        // ...and an explicit --server flag in turn beats any env value
        // (flags overwrite the initialized default), asserted parser-side.
    }
}
