//! `argv[0]` multicall dispatch — k3s `bin/k3s` parity (TODO **T0.1**).
//!
//! k3s selects behavior from the invoked program name; init-pro does the same.
//! One binary is deployed under many names (symlinks / hard links / copies),
//! and the basename of `argv[0]` decides what to run.
#![forbid(unsafe_code)]

use std::path::Path;

/// Behavior selected from `argv[0]`'s basename.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    /// `init-pro` itself (top-level CLI driver).
    InitPro,
    Server,
    Agent,
    /// Bundled-peer aliases (external CLIs) — Phase 1 stubs.
    Kubectl,
    Ctr,
    Crictl,
    Containerd,
    Etcd,
}

impl Action {
    /// Canonical name of an action (used for reexec + messages).
    pub const fn as_str(self) -> &'static str {
        match self {
            Action::InitPro => "init-pro",
            Action::Server => "server",
            Action::Agent => "agent",
            Action::Kubectl => "kubectl",
            Action::Ctr => "ctr",
            Action::Crictl => "crictl",
            Action::Containerd => "containerd",
            Action::Etcd => "etcd",
        }
    }

    /// True for bundled-peer CLI aliases (external CLIs). kubectl has had a
    /// real in-repo implementation since T3.1b but is still resolved — and
    /// classified — as a peer alias; the rest remain stubs.
    pub const fn is_external(self) -> bool {
        matches!(
            self,
            Action::Kubectl | Action::Ctr | Action::Crictl | Action::Containerd | Action::Etcd
        )
    }
}

/// Alias table: `argv[0]` basename -> [`Action`].
///
/// Mirrors k3s's external-CLI set (`crictl`, `ctr`, `kubectl`) plus its own
/// `server`/`agent` verbs, and adds init-pro's internal names.
pub const ALIASES: &[(&str, Action)] = &[
    ("init-pro", Action::InitPro),
    ("init-pro-server", Action::Server),
    ("init-pro-agent", Action::Agent),
    ("server", Action::Server),
    ("agent", Action::Agent),
    ("kubectl", Action::Kubectl),
    ("ctr", Action::Ctr),
    ("crictl", Action::Crictl),
    ("containerd", Action::Containerd),
    ("etcd", Action::Etcd),
];

/// Lowercased, extension-less basename of an `argv[0]`.
pub fn argv0_basename(argv0: &str) -> String {
    Path::new(argv0)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(argv0)
        .to_lowercase()
}

/// Resolve an `argv[0]` (or explicit alias name) to an [`Action`].
pub fn resolve(argv0: &str) -> Option<Action> {
    let name = argv0_basename(argv0);
    ALIASES.iter().find(|(a, _)| *a == name).map(|(_, a)| *a)
}

/// Does this raw arg vector ask for help? (`-h` / `--help` / `help`)
pub fn wants_help<I, S>(args: I) -> bool
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    args.into_iter()
        .any(|a| matches!(a.as_ref(), "-h" | "--help" | "help"))
}

/// Handle a bundled-peer alias invocation.
///
/// `kubectl` left this path in T3.1b — it now has a real in-repo
/// implementation (`rollout status` over plain HTTP, Q21) and is dispatched
/// before the stub in `init-pro`'s main. The stub remains for
/// `ctr`/`crictl`/`containerd`/`etcd` until the bundling pipeline
/// (T0.2/T0.4) embeds them; until then we still answer `--help` with
/// exit-success so the multicall contract holds, and reject anything else
/// with a clear not-yet-implemented error.
pub fn external_stub(action: Action, args: &[String]) -> std::process::ExitCode {
    use std::process::ExitCode;
    debug_assert!(
        action.is_external(),
        "external_stub called for non-external {action:?}"
    );
    let name = action.as_str();
    if wants_help(args.iter()) {
        println!("init-pro {name} — bundled {name} stub (Phase 1)");
        println!();
        println!("This alias is reserved for the bundled {name} binary. The peer is");
        println!("embedded by the bundling pipeline (T0.2) and exposed with full CLI");
        println!("parity (T0.4). In Phase 1 it prints this help and exits.");
        println!();
        println!("Usage: {name} [args ...]");
        ExitCode::SUCCESS
    } else {
        eprintln!("init-pro: '{name}' is not implemented in Phase 1");
        eprintln!("  The bundled {name} arrives with T0.2; full CLI parity with T0.4.");
        ExitCode::from(2)
    }
}

/// Re-exec the current binary under a forced `argv[0]` (k3s `stageAndRun` parity).
///
/// On success this never returns (the process image is replaced). The
/// `Ok(Infallible)` is therefore unreachable; callers can treat the returned
/// error as a reexec failure.
#[cfg(unix)]
pub fn reexec_as(alias: &str, extra_args: &[String]) -> std::io::Result<std::convert::Infallible> {
    use std::os::unix::process::CommandExt;
    let exe = std::env::current_exe()?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg0(alias);
    cmd.args(extra_args);
    // exec() returns Err(io::Error) only on failure; it never returns on success.
    Err(cmd.exec())
}

#[cfg(not(unix))]
pub fn reexec_as(
    _alias: &str,
    _extra_args: &[String],
) -> std::io::Result<std::convert::Infallible> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "reexec is only supported on unix",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_all_documented_aliases() {
        for name in [
            "init-pro",
            "init-pro-server",
            "init-pro-agent",
            "server",
            "agent",
            "kubectl",
            "ctr",
            "crictl",
            "containerd",
            "etcd",
        ] {
            assert!(
                resolve(name).is_some(),
                "{name} should resolve to an Action"
            );
        }
    }

    #[test]
    fn unknown_does_not_resolve() {
        assert!(resolve("definitely-not-an-alias").is_none());
        assert!(resolve("kubectl-old").is_none());
    }

    #[test]
    fn basename_strips_dirs_and_case() {
        assert_eq!(argv0_basename("/usr/local/bin/kubectl"), "kubectl");
        assert_eq!(argv0_basename("./server"), "server");
        assert_eq!(argv0_basename("/opt/init-pro/bin/ETCD"), "etcd");
        assert_eq!(argv0_basename("crictl"), "crictl");
    }

    #[test]
    fn symlink_resolution_is_canonical() {
        // `ln -sf init-pro kubectl` => argv[0]=".../kubectl" -> Kubectl
        assert_eq!(resolve("kubectl"), Some(Action::Kubectl));
        assert_eq!(resolve("server"), Some(Action::Server));
        assert_eq!(resolve("agent"), Some(Action::Agent));
        assert_eq!(resolve("init-pro-server"), Some(Action::Server));
        assert_eq!(resolve("init-pro-agent"), Some(Action::Agent));
        assert_eq!(resolve("init-pro"), Some(Action::InitPro));
    }

    #[test]
    fn alias_table_covers_all_actions() {
        let covered: Vec<Action> = ALIASES.iter().map(|(_, a)| *a).collect();
        let all = [
            Action::InitPro,
            Action::Server,
            Action::Agent,
            Action::Kubectl,
            Action::Ctr,
            Action::Crictl,
            Action::Containerd,
            Action::Etcd,
        ];
        for a in all {
            assert!(covered.contains(&a), "{a:?} missing from ALIASES");
        }
    }

    #[test]
    fn wants_help_detects_all_forms() {
        assert!(wants_help(["--help".to_string()]));
        assert!(wants_help(["-h".to_string()]));
        assert!(wants_help(["server".to_string(), "help".to_string()]));
        assert!(!wants_help(["server".to_string()]));
    }

    #[test]
    fn external_flag_matches_alias_set() {
        assert!(Action::Kubectl.is_external());
        assert!(Action::Etcd.is_external());
        assert!(!Action::Server.is_external());
        assert!(!Action::InitPro.is_external());
    }

    #[test]
    fn as_str_returns_canonical_name_for_each_variant() {
        // Every variant must map to its canonical reexec name; a cross-wire
        // here would break reexec_as and the help banner.
        assert_eq!(Action::InitPro.as_str(), "init-pro");
        assert_eq!(Action::Server.as_str(), "server");
        assert_eq!(Action::Agent.as_str(), "agent");
        assert_eq!(Action::Kubectl.as_str(), "kubectl");
        assert_eq!(Action::Ctr.as_str(), "ctr");
        assert_eq!(Action::Crictl.as_str(), "crictl");
        assert_eq!(Action::Containerd.as_str(), "containerd");
        assert_eq!(Action::Etcd.as_str(), "etcd");
    }

    #[test]
    fn each_alias_maps_to_its_expected_action() {
        // Guard against a cross-wire typo in ALIASES (e.g. "ctr" -> Etcd),
        // which the prior is_some()-only test would have missed.
        for (name, expected) in ALIASES {
            assert_eq!(
                resolve(name),
                Some(*expected),
                "alias {name:?} must resolve to {expected:?}"
            );
        }
        // Explicit spot-checks for the previously under-asserted peers.
        assert_eq!(resolve("ctr"), Some(Action::Ctr));
        assert_eq!(resolve("crictl"), Some(Action::Crictl));
        assert_eq!(resolve("containerd"), Some(Action::Containerd));
        assert_eq!(resolve("etcd"), Some(Action::Etcd));
    }
}
