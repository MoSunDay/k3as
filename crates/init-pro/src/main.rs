//! `init-pro` — the single binary.
//!
//! Behavior is selected by `argv[0]` (multicall, TODO **T0.1**). This file is
//! intentionally tiny: resolve the basename to an [`Action`] and hand off to
//! the CLI driver or the external-peer stub.

#![forbid(unsafe_code)]

use std::process::ExitCode;

use common::embed::EmbeddedManifest;
use multicall::{resolve, Action};

mod assets;

/// T4.3 Scope A (Q34): the embedded `flannel` CNI meta-plugin. containerd
/// execs the `<runtime>/aux/flannel` symlink (argv[0] dispatch) and `init-pro
/// flannel` surfaces it k3s-style; CNI arguments come from the process
/// environment and the netconf from stdin, so argv is ignored beyond a
/// help nicety (must bypass clap — `skel::run` owns stdin).
fn run_flannel_cni(extra: &[String]) -> ExitCode {
    if extra.iter().any(|a| a == "-h" || a == "--help" || a == "help") {
        println!(
            "Usage (CNI plugin — invoked by containerd, not by hand):\n\
             \x20 CNI_COMMAND=ADD|DEL|CHECK|VERSION  CNI_CONTAINERID=... \\\n\
             \x20   netconf.json | flannel\n\
             \n\
             Reads the flannel netconf from stdin and the CNI_*, path and\n\
             FLANNEL_SUBNET_FILE variables from the environment; writes the\n\
             CNI result (or error) JSON to stdout."
        );
        return ExitCode::SUCCESS;
    }
    ExitCode::from(flannel_cni::skel::run() as u8)
}

fn main() -> ExitCode {
    let manifest = EmbeddedManifest {
        assets: assets::embedded_assets(),
        sha256_sums: assets::SHA256_SUMS,
        data_links: assets::DATA_LINKS,
    };

    // args().next() is argv[0] as the kernel/shell provided it (NOT the
    // resolved binary path), so a symlink named `kubectl` lands here as
    // ".../kubectl".
    let argv0 = std::env::args().next().unwrap_or_default();
    let extra: Vec<String> = std::env::args().skip(1).collect();

    // T4.1 (Q25): `init-pro crictl ps` / `init-pro ctr ...` passthrough —
    // intercepted pre-clap (k3s-style subcommand surface) so peer flags
    // never touch the flag strip filter; the multicall seam re-execs the
    // staged binary with the agent socket injected.
    if let Some(action) = match extra.first().map(String::as_str) {
        Some("crictl") => Some(Action::Crictl),
        Some("ctr") => Some(Action::Ctr),
        _ => None,
    } {
        return multicall::external_stub(action, &extra[1..]);
    }

    // T4.3 Scope A (Q34): `init-pro flannel` passthrough (pre-clap, like
    // crictl/ctr above) — the CNI plugin must own stdin for the netconf.
    if extra.first().map(String::as_str) == Some("flannel") {
        return run_flannel_cni(&extra[1..]);
    }

    match resolve(&argv0) {
        // init-pro itself (or an unknown name) -> drive the CLI; clap will
        // print help/version as appropriate.
        Some(Action::InitPro) | None => cli::run(&manifest),
        // Forced init-pro subcommands via alias name.
        Some(Action::Server) => cli::run_forced("server", &extra, &manifest),
        Some(Action::Agent) => cli::run_forced("agent", &extra, &manifest),
        // kubectl grew a real in-repo surface in T3.1b (`rollout status`
        // over plain HTTP, Q21) — split out of the external_stub catch-all.
        Some(Action::Kubectl) => kubectl::run(&extra),
        // The flannel CNI meta-plugin, whether invoked through the staged
        // `<runtime>/aux/flannel` symlink (argv[0]) or as a subcommand.
        Some(Action::Flannel) => run_flannel_cni(&extra),
        // Bundled-peer aliases: Phase 1 stubs.
        Some(external) => multicall::external_stub(external, &extra),
    }
}
