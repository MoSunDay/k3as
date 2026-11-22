//! ClusterIP VIP bind mechanism (TODO **T4.3 Scope C**, decision **Q44**,
//! Sprint 51): alias the Service CIDR's VIPs onto `lo` as `/32`s so the
//! clusterIP listener plane can bind them and in-cluster clients get a
//! local route to the VIP.
//!
//! This is the klipper/k3s precedent (svclb <-> `ip addr add <vip>/32 dev
//! lo`): chosen over `IP_FREEBIND` because the alias fixes BOTH halves —
//! the `bind(2)` on a not-yet-local address AND the local routing (a
//! FREEBIND socket could bind but traffic to the VIP would still leave the
//! host without a route). Everything here is best-effort-by-contract: a
//! missing `ip(1)` or a missing `NET_ADMIN` capability degrades to one
//! warn per VIP and the listener is skipped ([`crate::clusterip`]); it
//! never fails the plane.

use std::net::Ipv4Addr;

/// argv for `ip addr add|del <ip>/32 dev lo` (pure; unit-tested).
pub(crate) fn alias_args(ip: Ipv4Addr, add: bool) -> Vec<String> {
    vec![
        "addr".into(),
        if add { "add" } else { "del" }.into(),
        format!("{ip}/32"),
        "dev".into(),
        "lo".into(),
    ]
}

/// Classify `ip` output as "the alias is already present" (idempotent add):
/// classic iproute2 answers `RTNETLINK answers: File exists`; newer
/// releases (v6+) spell it `Error: ipv4: Address already assigned`.
pub(crate) fn is_already_exists(output: &str) -> bool {
    output.contains("File exists") || output.contains("Address already assigned")
}

/// Ensure the `/32` alias for `ip` exists on `lo`.
///
/// `Ok(true)` when the command succeeded OR the address was already
/// present (idempotent); `Err(msg)` otherwise — missing binary, permission
/// denial (`NET_ADMIN`), or any other iproute2 failure, with the trimmed
/// stderr folded into the message.
pub(crate) async fn ensure_alias(ip: Ipv4Addr) -> Result<bool, String> {
    let args = alias_args(ip, true);
    let out = match tokio::process::Command::new("ip")
        .args(&args)
        .output()
        .await
    {
        Ok(out) => out,
        Err(e) => {
            return Err(format!(
                "spawning ip(1) for VIP {ip} failed ({e}); VIP aliasing needs iproute2"
            ))
        }
    };
    if out.status.success() {
        return Ok(true);
    }
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    if is_already_exists(&text) {
        return Ok(true);
    }
    Err(format!(
        "ip addr add {ip}/32 dev lo failed: {}",
        text.trim()
    ))
}

/// Best-effort alias removal (plane teardown): failures are debug-logged
/// and swallowed — a stale `/32` on `lo` is harmless at shutdown.
pub(crate) async fn remove_alias(ip: Ipv4Addr) {
    let args = alias_args(ip, false);
    match tokio::process::Command::new("ip")
        .args(&args)
        .output()
        .await
    {
        Ok(out) if out.status.success() => {}
        Ok(out) => tracing::debug!(
            target: "init-pro",
            vip = %ip,
            output = %String::from_utf8_lossy(&out.stderr).trim(),
            "clusterIP alias removal no-op/failed"
        ),
        Err(e) => tracing::debug!(
            target: "init-pro",
            vip = %ip,
            error = %e,
            "clusterIP alias removal spawn failed"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alias_args_exact_argv_with_slash32() {
        let ip: Ipv4Addr = "10.43.0.7".parse().unwrap();
        assert_eq!(
            alias_args(ip, true),
            vec!["addr", "add", "10.43.0.7/32", "dev", "lo"]
        );
        assert_eq!(
            alias_args(ip, false),
            vec!["addr", "del", "10.43.0.7/32", "dev", "lo"]
        );
    }

    #[test]
    fn already_exists_classifier() {
        // iproute2's duplicate-alias answer counts as already-present —
        // both the classic and the v6+ wording.
        assert!(is_already_exists("RTNETLINK answers: File exists\n"));
        assert!(is_already_exists(
            "Warning: ... \nRTNETLINK answers: File exists"
        ));
        assert!(is_already_exists(
            "Error: ipv4: Address already assigned.\n"
        ));
        // Everything else (permission denied, bad address, silence) does not.
        assert!(!is_already_exists(
            "RTNETLINK answers: Operation not permitted"
        ));
        assert!(!is_already_exists("Error: argument \"lo\" is wrong"));
        assert!(!is_already_exists(""));
    }
}
