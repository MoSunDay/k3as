//! `init-pro` — the single binary.
//!
//! Behavior is selected by `argv[0]` (multicall, TODO **T0.1**). This file is
//! intentionally tiny: resolve the basename to an [`Action`] and hand off to
//! the CLI driver or the external-peer stub.

#![forbid(unsafe_code)]

use std::process::ExitCode;

use init_pro_multicall::{resolve, Action};

fn main() -> ExitCode {
    // args().next() is argv[0] as the kernel/shell provided it (NOT the
    // resolved binary path), so a symlink named `kubectl` lands here as
    // ".../kubectl".
    let argv0 = std::env::args().next().unwrap_or_default();
    let extra: Vec<String> = std::env::args().skip(1).collect();

    match resolve(&argv0) {
        // init-pro itself (or an unknown name) -> drive the CLI; clap will
        // print help/version as appropriate.
        Some(Action::InitPro) | None => init_pro_cli::run(),
        // Forced init-pro subcommands via alias name.
        Some(Action::Server) => init_pro_cli::run_forced("server", &extra),
        Some(Action::Agent) => init_pro_cli::run_forced("agent", &extra),
        // Bundled-peer aliases: Phase 1 stubs.
        Some(external) => init_pro_multicall::external_stub(external, &extra),
    }
}
