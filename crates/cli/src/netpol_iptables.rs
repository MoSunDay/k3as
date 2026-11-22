//! iptables data plane for NetworkPolicy enforcement (TODO **T4.4**,
//! decision **Q64**): renders the compiled [`NetpolProgram`] into the
//! kernel and tears it down with zero residue (NP4).
//!
//! Chain plan (one namespace of chains, all under `INIT-PRO-NPOL-`):
//! - `INIT-PRO-NPOL-FWD` dispatch: one `-d/-s <podIP>/32` jump per
//!   isolated pod + a terminal `-j RETURN`; jumped from `FORWARD`
//!   position 1 via `iptables -I` (guarded by `-C`) — inserted by exec,
//!   NOT via restore, so a restore failure can never duplicate or
//!   reorder the hook (pod->pod bridged traffic reaches FORWARD through
//!   `bridge-nf-call-iptables`).
//! - per-pod `INIT-PRO-NPOL-{I,E}<12hex>` chains: conntrack ACCEPT +
//!   allow rules + terminal `-j REJECT`.
//!
//! Full regen is ONE `iptables-restore --noflush` payload: `:chain -`
//! declarations + explicit `-F` lines (with `--noflush` a declaration
//! alone does NOT flush an existing chain — the `-F` lines make the
//! regen idempotent), stale-chain prune (`-F` + `-X` for chains the new
//! program no longer owns, parsed from `iptables-save -t filter`), then
//! the `-A` rules — atomic per table, no windows with missing rules.
//! Deny is `REJECT`, not `DROP` (fail-fast probes, Q64).
//!
//! The whole file shells out through PATH (`iptables`,
//! `iptables-restore`, `iptables-save`, nf_tables or legacy — v1.8.x
//! verified); failures surface as `Err` and the keeper degrades to a
//! WARN (enforcement pauses, it never takes the agent down).

use std::collections::BTreeSet;
use std::io;
use std::process::Stdio;

use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::netpol_compile::{NetpolProgram, CHAIN_PREFIX, DISPATCH_CHAIN};

/// Ensure the FORWARD -> dispatch jump exists exactly once (`-C` guard,
/// then `-I FORWARD 1`). By exec, never via restore (see module docs).
pub(crate) async fn ensure_dispatch_jump() -> io::Result<()> {
    if Command::new("iptables")
        .args(["-C", "FORWARD", "-j", DISPATCH_CHAIN])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
    {
        return Ok(()); // already hooked
    }
    let out = Command::new("iptables")
        .args(["-I", "FORWARD", "1", "-j", DISPATCH_CHAIN])
        .output()
        .await?;
    status_ok(&out, "iptables -I FORWARD")
}

/// Every `INIT-PRO-NPOL-*` chain currently in the `filter` table
/// (parsed from `iptables-save -t filter`; `Err` on any tool failure —
/// the caller then skips pruning rather than guessing).
pub(crate) async fn current_npol_chains() -> io::Result<BTreeSet<String>> {
    let out = Command::new("iptables-save")
        .args(["-t", "filter"])
        .output()
        .await?;
    status_ok(&out, "iptables-save")?;
    let text = String::from_utf8_lossy(&out.stdout);
    Ok(text
        .lines()
        .filter_map(|l| l.strip_prefix(':'))
        .map(|l| l.split_whitespace().next().unwrap_or(""))
        .filter(|name| name.starts_with(CHAIN_PREFIX))
        .map(str::to_string)
        .collect())
}

/// Apply a full regen atomically: flush+refill every program chain,
/// prune chains the program no longer owns, leave foreign rules alone.
pub(crate) async fn apply_program(prog: &NetpolProgram) -> io::Result<()> {
    let mut payload = String::from("*filter\n");
    for chain in prog.chains.keys() {
        payload.push_str(&format!(":{chain} - [0:0]\n-F {chain}\n"));
    }
    for stale in current_npol_chains()
        .await
        .unwrap_or_default()
        .difference(&prog.chains.keys().cloned().collect::<BTreeSet<_>>())
    {
        // Stale = present in the kernel, absent from the new program.
        // Declare (no-op when it exists), flush, then delete — chains
        // are only ever referenced by the dispatch chain, already
        // flushed above in this same payload.
        payload.push_str(&format!(":{stale} - [0:0]\n-F {stale}\n-X {stale}\n"));
    }
    for (chain, rules) in &prog.chains {
        for rule in rules {
            payload.push_str(&format!("-A {chain} {rule}\n"));
        }
    }
    payload.push_str("COMMIT\n");
    restore(&payload).await
}

/// Remove every T4.4 artifact: the FORWARD jump (if present) and every
/// `INIT-PRO-NPOL-*` chain (flush + delete). Idempotent — a clean host
/// applies an empty payload and succeeds (zero-residue NP4 posture).
pub(crate) async fn teardown() -> io::Result<()> {
    let chains = current_npol_chains().await.unwrap_or_default();
    let jump = Command::new("iptables")
        .args(["-C", "FORWARD", "-j", DISPATCH_CHAIN])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false);
    let mut payload = String::from("*filter\n");
    if jump {
        payload.push_str(&format!("-D FORWARD -j {DISPATCH_CHAIN}\n"));
    }
    for chain in &chains {
        payload.push_str(&format!("-F {chain}\n-X {chain}\n"));
    }
    payload.push_str("COMMIT\n");
    restore(&payload).await
}

/// Feed one `iptables-restore --noflush` payload (module-private: all
/// callers go through [`apply_program`]/[`teardown`], which own the
/// payload shapes).
async fn restore(payload: &str) -> io::Result<()> {
    let mut child = Command::new("iptables-restore")
        .arg("--noflush")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(payload.as_bytes()).await?;
        stdin.shutdown().await?;
    }
    let out = child.wait_with_output().await?;
    status_ok(&out, "iptables-restore")
}

fn status_ok(out: &std::process::Output, what: &str) -> io::Result<()> {
    if out.status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "{what} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The chain namespace and name lengths stay inside iptables' 28-char
    /// limit for every per-pod chain shape (compile owns the suffix).
    #[test]
    fn chain_names_fit_iptables_limit() {
        let suffix = crate::netpol_compile::chain_suffix("11111111-2222-3333-4444-555555555555");
        assert_eq!(suffix.len(), 12);
        assert_eq!(suffix, "111111112222");
        for kind in ["I", "E"] {
            let name = format!("{CHAIN_PREFIX}{kind}{suffix}");
            assert!(name.len() <= 28, "{name} too long");
            assert!(name.starts_with(CHAIN_PREFIX));
        }
        assert_eq!(DISPATCH_CHAIN, "INIT-PRO-NPOL-FWD");
    }
}
