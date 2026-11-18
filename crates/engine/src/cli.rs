//! Command-line surface for the internal engine process. Deliberately
//! hand-parsed: the engine must not pull clap (or anything that could
//! spawn a thread) into its closure.

use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use nsrun::{BindMount, CgroupLimits, Output, RunSpec};

use crate::server;

pub fn run_serve(args: &[String]) -> i32 {
    let (socket, opts) = match parse_serve(args) {
        Ok(v) => v,
        // `-h`/`--help` is a request, not a usage error.
        Err(e) if e == "help requested" => {
            crate::print_usage();
            return 0;
        }
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    match socket {
        Some(path) => server::serve(&path, &opts),
        None => {
            eprintln!("engine serve: --socket PATH is required");
            2
        }
    }
}

/// Parses the `serve` flag surface. Returns `Err("help requested")` for
/// `-h`/`--help` so the caller can print usage without treating a help
/// request as a usage error.
fn parse_serve(args: &[String]) -> Result<(Option<PathBuf>, server::ServeOptions), String> {
    let mut socket = None;
    let mut opts = server::ServeOptions::default();
    let mut i = 0;
    let value = |i: &mut usize, flag: &str| -> Result<String, String> {
        *i += 1;
        args.get(*i)
            .cloned()
            .ok_or_else(|| format!("engine serve: {flag} needs a value"))
    };
    while i < args.len() {
        let r = match args[i].as_str() {
            "--socket" => value(&mut i, "--socket").map(|v| socket = Some(PathBuf::from(v))),
            "--pause" => {
                opts.pause = true;
                Ok(())
            }
            "--host-net" => {
                opts.host_net = true;
                Ok(())
            }
            "--netns" => value(&mut i, "--netns").map(|v| opts.netns = Some(v)),
            "--log-path" => value(&mut i, "--log-path").map(|v| opts.log_path = Some(v)),
            "--mount" => value(&mut i, "--mount")
                .and_then(|v| parse_serve_mount(&v).map(|m| opts.mounts.push(m))),
            "-h" | "--help" => return Err("help requested".to_string()),
            other => Err(format!("engine serve: unexpected argument {other:?}")),
        };
        r?;
        i += 1;
    }
    // Q52: --host-net keeps the sandbox in the host netns; --netns joins a
    // specific one. Both at once is contradictory — fail fast.
    if opts.host_net && opts.netns.is_some() {
        return Err(
            "engine serve: --host-net and --netns are mutually exclusive (Q52)".to_string(),
        );
    }
    Ok((socket, opts))
}

/// `SRC:DST[:ro]`, the same shape engine-run uses.
fn parse_serve_mount(raw: &str) -> Result<engine_proto::MountSpec, String> {
    let parts: Vec<&str> = raw.split(':').collect();
    let (source, target, readonly) = match parts.as_slice() {
        [s, t] => (s.to_string(), t.to_string(), false),
        [s, t, "ro"] => (s.to_string(), t.to_string(), true),
        _ => return Err(format!("engine serve: bad --mount {raw:?} (SRC:DST[:ro])")),
    };
    Ok(engine_proto::MountSpec {
        source,
        target,
        readonly,
    })
}

pub fn run_once(args: &[String]) -> i32 {
    let (spec, command) = match parse(args) {
        Ok(v) => v,
        // `-h`/`--help` is a request, not a usage error.
        Err(e) if e == "help requested" => {
            crate::print_usage();
            return 0;
        }
        Err(e) => {
            eprintln!("engine-run: {e}");
            return 2;
        }
    };
    let _ = command;
    if let Err(e) = nsrun::preflight(!spec.cgroup.is_empty()) {
        eprintln!("engine-run: {e}");
        return 125;
    }
    let mut child = match nsrun::spawn(&spec) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("engine-run: {e}");
            return 125;
        }
    };
    loop {
        match child.next_event(Duration::from_millis(100)) {
            Ok(Some(Output::Stdout(bytes))) => {
                let _ = std::io::stdout().write_all(&bytes);
                let _ = std::io::stdout().flush();
            }
            Ok(Some(Output::Stderr(bytes))) => {
                let _ = std::io::stderr().write_all(&bytes);
                let _ = std::io::stderr().flush();
            }
            Ok(Some(Output::Exited(code))) => {
                child.cleanup();
                return code;
            }
            Ok(None) => {}
            Err(e) => {
                eprintln!("engine-run: {e}");
                child.kill();
                let _ = child.wait();
                child.cleanup();
                return 125;
            }
        }
    }
}

fn parse(args: &[String]) -> Result<(RunSpec, Vec<String>), String> {
    let mut rootfs: Option<PathBuf> = None;
    let mut hostname = None;
    let mut uid = 0u32;
    let mut gid = 0u32;
    let mut env = Vec::new();
    let mut mounts: Vec<BindMount> = Vec::new();
    let mut cgroup_name = None;
    let mut limits = CgroupLimits::default();
    let mut new_net = false;
    let mut netns: Option<PathBuf> = None;
    let mut command = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if arg == "-h" || arg == "--help" {
            return Err("help requested".to_string());
        }
        if arg == "--" {
            command.extend_from_slice(&args[i + 1..]);
            break;
        }
        if !arg.starts_with("--") {
            command.extend_from_slice(&args[i..]);
            break;
        }
        // `--cgroup=NAME` parses like the space form and is held to the same
        // value rule (an empty NAME is an error, not a default).
        if let Some(value) = arg.strip_prefix("--cgroup=") {
            cgroup_name = Some(parse_cgroup_value(value)?);
            i += 1;
            continue;
        }
        match arg.as_str() {
            "--rootfs" | "--hostname" | "--uid" | "--gid" | "--env" | "--mount" | "--cgroup"
            | "--memory-max" | "--cpu-max" | "--pids-max" | "--netns" => {
                i += 1;
                let value = args
                    .get(i)
                    .ok_or_else(|| format!("{arg} needs a value"))?
                    .clone();
                match arg.as_str() {
                    "--rootfs" => rootfs = Some(PathBuf::from(value)),
                    "--hostname" => hostname = Some(value),
                    "--uid" => uid = parse_u32("--uid", &value)?,
                    "--gid" => gid = parse_u32("--gid", &value)?,
                    "--env" => env.push(value),
                    "--mount" => mounts.push(parse_mount(&value)?),
                    "--cgroup" => cgroup_name = Some(parse_cgroup_value(&value)?),
                    "--memory-max" => limits.memory_max = Some(parse_u64(arg, &value)?),
                    "--cpu-max" => limits.cpu_max = Some(value),
                    "--pids-max" => limits.pids_max = Some(parse_u64(arg, &value)?),
                    "--netns" => netns = Some(PathBuf::from(value)),
                    _ => unreachable!(),
                }
            }
            "--new-net" => new_net = true,
            other => return Err(format!("unknown flag {other:?}")),
        }
        i += 1;
    }
    let rootfs = rootfs.ok_or_else(|| "--rootfs DIR is required".to_string())?;
    if command.is_empty() {
        return Err("no command (use `-- COMMAND [ARG]...`)".to_string());
    }
    let mut spec = RunSpec::new(rootfs, command.clone());
    spec.hostname = hostname;
    spec.uid = uid;
    spec.gid = gid;
    spec.env = env;
    spec.mounts = mounts;
    spec.new_net = new_net;
    spec.netns = netns;
    let has_limits = !limits.is_empty();
    // `cgroup_name` is only meaningful as the home of limits: `nsrun::spawn`
    // skips the cgroup entirely without them, so accepting the flag would be
    // a silent no-op (and a silent missing memory cap).
    if cgroup_name.is_some() && !has_limits {
        return Err(
            "--cgroup NAME has no effect without --memory-max/--cpu-max/--pids-max".to_string(),
        );
    }
    if has_limits {
        spec.cgroup = limits;
        spec.cgroup_name =
            Some(cgroup_name.unwrap_or_else(|| format!("run-{}", std::process::id())));
    }
    spec.validate().map_err(|e| e.to_string())?;
    Ok((spec, command))
}

/// Shared by `--cgroup NAME` and `--cgroup=NAME`: a name is never optional
/// once the flag is present.
fn parse_cgroup_value(value: &str) -> Result<String, String> {
    if value.is_empty() {
        return Err("--cgroup wants a non-empty NAME".to_string());
    }
    Ok(value.to_string())
}

fn parse_mount(value: &str) -> Result<BindMount, String> {
    let parts: Vec<&str> = value.split(':').collect();
    match parts.as_slice() {
        [src, dst] => Ok(BindMount::new(*src, *dst)),
        [src, dst, "ro"] => Ok(BindMount::new(*src, *dst).readonly()),
        _ => Err(format!("--mount wants SRC:DST[:ro], got {value:?}")),
    }
}

fn parse_u32(flag: &str, value: &str) -> Result<u32, String> {
    value
        .parse()
        .map_err(|_| format!("{flag} wants an integer, got {value:?}"))
}

fn parse_u64(flag: &str, value: &str) -> Result<u64, String> {
    value
        .parse()
        .map_err(|_| format!("{flag} wants an integer, got {value:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_the_smoke_invocation() {
        let args = args(&[
            "--rootfs",
            "/tmp/rootfs",
            "--hostname",
            "demo",
            "--env",
            "A=1",
            "--mount",
            "/etc/hosts:/etc/hosts:ro",
            "--",
            "/bin/echo",
            "hi",
        ]);
        let (spec, argv) = parse(&args).unwrap();
        assert_eq!(spec.rootfs, PathBuf::from("/tmp/rootfs"));
        assert_eq!(spec.hostname.as_deref(), Some("demo"));
        assert_eq!(spec.env, ["A=1"]);
        assert!(spec.mounts[0].readonly);
        assert_eq!(argv, ["/bin/echo", "hi"]);
    }

    #[test]
    fn rejects_missing_rootfs_and_command() {
        assert!(parse(&[]).is_err());
        let args = vec!["--rootfs".to_string(), "/r".to_string()];
        assert!(parse(&args).is_err());
    }

    #[test]
    fn serve_flags_parse_host_net() {
        let (socket, opts) =
            parse_serve(&args(&["--socket", "/tmp/s.sock", "--pause", "--host-net"]))
                .expect("serve flags");
        assert_eq!(
            socket.map(|p| p.display().to_string()).as_deref(),
            Some("/tmp/s.sock")
        );
        assert!(opts.pause && opts.host_net);
        assert!(opts.netns.is_none());
    }

    #[test]
    fn serve_host_net_defaults_off() {
        let (_, opts) = parse_serve(&args(&["--socket", "/tmp/s2.sock"])).expect("serve flags");
        assert!(!opts.pause && !opts.host_net);
    }

    #[test]
    fn serve_host_net_and_netns_are_mutually_exclusive() {
        let both = args(&[
            "--socket",
            "/tmp/s.sock",
            "--pause",
            "--host-net",
            "--netns",
            "/proc/1/ns/net",
        ]);
        let err = match parse_serve(&both) {
            Ok(_) => panic!("host-net + netns must conflict"),
            Err(e) => e,
        };
        assert!(err.contains("mutually exclusive"), "{err}");
    }

    #[test]
    fn serve_conflicting_net_flags_exit_two() {
        let rc = run_serve(&args(&[
            "--socket",
            "/tmp/s4.sock",
            "--pause",
            "--host-net",
            "--netns",
            "/proc/1/ns/net",
        ]));
        assert_eq!(rc, 2);
    }

    #[test]
    fn help_exits_zero_and_unknown_flags_exit_two() {
        assert_eq!(run_once(&args(&["--help"])), 0);
        assert_eq!(run_once(&args(&["-h"])), 0);
        assert_eq!(run_once(&args(&["--unknown-flag"])), 2);
    }

    #[test]
    fn cgroup_without_limits_is_an_error() {
        let space = args(&["--rootfs", "/r", "--cgroup", "demo", "--", "/bin/true"]);
        let err = parse(&space).expect_err("cgroup needs limits");
        assert!(err.contains("no effect"), "{err}");
        let equals = args(&["--rootfs", "/r", "--cgroup=demo", "--", "/bin/true"]);
        assert!(parse(&equals).is_err());
    }

    #[test]
    fn cgroup_equals_form_parses_with_limits() {
        let list = args(&[
            "--rootfs",
            "/r",
            "--cgroup=demo",
            "--pids-max",
            "16",
            "--",
            "/bin/true",
        ]);
        let (spec, _) = parse(&list).unwrap();
        assert_eq!(spec.cgroup_name.as_deref(), Some("demo"));
        assert_eq!(spec.cgroup.pids_max, Some(16));
        // An empty NAME is not a default.
        let empty = args(&["--rootfs", "/r", "--cgroup=", "--", "/bin/true"]);
        assert!(parse(&empty).is_err());
    }

    #[test]
    fn parses_mount_modes() {
        assert!(!parse_mount("/a:/a").unwrap().readonly);
        assert!(parse_mount("/a:/a:ro").unwrap().readonly);
        assert!(parse_mount("/a").is_err());
    }
}
