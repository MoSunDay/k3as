//! `INIT_PRO_DISABLE_ANYHOME` env path for `init-pro server --disable-anyhome`
//! (T-anyhome, Q36).
//!
//! These tests live in their OWN integration-test binary on purpose: they
//! mutate the process environment, and the `cli` unit-test binary runs many
//! other clap parses on parallel threads that would observe (and be broken
//! by) a leaked fixture value. A dedicated binary isolates them from the
//! rest of the suite; the mutex below serializes them against each other.

use std::sync::Mutex;

use clap::Parser;
use cli::{Cli, Command};

/// Serialize every test that touches `INIT_PRO_DISABLE_ANYHOME` (clap reads
/// the env at parse time).
static ENV_LOCK: Mutex<()> = Mutex::new(());

const VAR: &str = "INIT_PRO_DISABLE_ANYHOME";

/// Parse `init-pro server ...` and report the `disable_anyhome` field.
fn parse_disable_anyhome(argv: &[&str]) -> Result<bool, String> {
    let cli = Cli::try_parse_from(std::iter::once("init-pro").chain(argv.iter().copied()))
        .map_err(|e| e.to_string())?;
    match cli.command {
        Some(Command::Server(s)) => Ok(s.disable_anyhome),
        other => Err(format!("expected Server subcommand, got {other:?}")),
    }
}

#[test]
fn env_true_disables_anyhome() {
    let _g = ENV_LOCK.lock().unwrap();
    std::env::set_var(VAR, "true");
    let disabled = parse_disable_anyhome(&["server"]).expect("env true parses");
    std::env::remove_var(VAR);
    assert!(disabled, "INIT_PRO_DISABLE_ANYHOME=true must disable anyhome");
}

/// ACTUAL clap v4 behavior (4.6): a `bool` field is `ArgAction::SetTrue` and
/// its env value goes through the bool value parser, so ONLY the exact
/// spellings `true`/`false` are accepted. The numeric `1`/`0` forms (unlike
/// k3s' own env handling) and any other casing (`TRUE`, `FALSE`) plus the
/// boolish `yes`/`on` spellings are REJECTED.
#[test]
fn env_numeric_and_boolish_and_cased_forms_are_rejected() {
    let _g = ENV_LOCK.lock().unwrap();
    for v in ["1", "0", "TRUE", "FALSE", "yes", "on"] {
        std::env::set_var(VAR, v);
        let err = parse_disable_anyhome(&["server"])
            .expect_err("non-`true`/`false` env must fail the parse");
        std::env::remove_var(VAR);
        assert!(
            err.contains("invalid value") && err.contains("true, false"),
            "expected a bool value-parser rejection for {v:?}, got: {err}"
        );
    }
}

#[test]
fn env_false_keeps_anyhome_enabled() {
    let _g = ENV_LOCK.lock().unwrap();
    std::env::set_var(VAR, "false");
    let disabled = parse_disable_anyhome(&["server"]).expect("env false parses");
    std::env::remove_var(VAR);
    assert!(
        !disabled,
        "INIT_PRO_DISABLE_ANYHOME=false must keep anyhome enabled"
    );
}

#[test]
fn env_absent_and_no_flag_keeps_anyhome_enabled() {
    let _g = ENV_LOCK.lock().unwrap();
    std::env::remove_var(VAR);
    let disabled = parse_disable_anyhome(&["server"]).expect("bare server parses");
    assert!(!disabled, "env absent + no flag must keep anyhome enabled");
}

#[test]
fn explicit_flag_disables_anyhome() {
    let _g = ENV_LOCK.lock().unwrap();
    std::env::remove_var(VAR);
    let disabled =
        parse_disable_anyhome(&["server", "--disable-anyhome"]).expect("flag parses");
    assert!(disabled, "--disable-anyhome must disable anyhome");
}

#[test]
fn explicit_flag_overrides_env_false() {
    let _g = ENV_LOCK.lock().unwrap();
    std::env::set_var(VAR, "false");
    let disabled = parse_disable_anyhome(&["server", "--disable-anyhome"])
        .expect("flag beats env");
    std::env::remove_var(VAR);
    assert!(disabled, "argv --disable-anyhome must override env false");
}
