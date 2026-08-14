//! `kubectl` — the first real kubectl surface inside init-pro (T3.1b).
//!
//! Invoked via the multicall `argv[0]` dispatch (`ln -s init-pro kubectl`);
//! the CLI clap path is intentionally untouched (plan T3.1b: argv[0] only).
//!
//! v1 surface: `kubectl rollout status (deployment/NAME | deployment NAME)`.
//! Simplifications are locked by decision **Q21**:
//! - transport: plain HTTP/1.1 over a raw `tokio::TcpStream`, no auth, no
//!   https, no new dependencies (TLS + auth arrive with T1.3);
//! - waiting: poll GET every 250ms instead of a watch stream.

#![forbid(unsafe_code)]

mod http;
mod rollout;
#[cfg(test)]
mod rollout_poll_tests;

use std::process::ExitCode;
use std::time::Duration;

use http::HttpClient;
use rollout::{rollout_status, Outcome};

/// Default apiserver address (server default bind is 127.0.0.1:6443).
const DEFAULT_SERVER: &str = "http://127.0.0.1:6443";
/// Default namespace, kubectl parity.
const DEFAULT_NAMESPACE: &str = "default";

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

    let mut server = DEFAULT_SERVER.to_string();
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
        "v1 surface: rollout status for Deployments (T3.1b, Q21 plain-HTTP transport).".into(),
        String::new(),
        "Usage:".into(),
        "  kubectl rollout status (TYPE/NAME | TYPE NAME) [flags]".into(),
        String::new(),
        "Target forms: deployment/NAME, deployments/NAME, deploy/NAME (deployment only in v1)."
            .into(),
        String::new(),
        "Flags:".into(),
        "  -h, --help                 help for kubectl".into(),
        format!(
            "      --server string       http address of the apiserver (default \"{DEFAULT_SERVER}\")"
        ),
        format!("  -n, --namespace string     namespace scope (default \"{DEFAULT_NAMESPACE}\")"),
        "      --timeout duration    max wait, e.g. 30s, 5m, or bare seconds; 0 = poll once".into(),
        "                            (default: wait forever)".into(),
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
    }
}
