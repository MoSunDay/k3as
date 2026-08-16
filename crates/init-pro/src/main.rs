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
        // Bundled-peer aliases: Phase 1 stubs.
        Some(external) => multicall::external_stub(external, &extra),
    }
}
