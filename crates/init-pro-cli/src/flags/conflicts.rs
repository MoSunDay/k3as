//! Fatal conflict rules + `--disable` validation (Q9 matrix Table B, A4).
//!
//! Ported verbatim from k3s `pkg/cli/server/server.go:245-265` (server
//! conflicts), `pkg/cli/cmds/stage.go:9` (`DisableItems`), and
//! `pkg/cli/agent/agent.go:83-87` (agent token/server). Each violation exits
//! non-zero with the k3s-parity message.

use crate::cmd::{AgentCmd, ServerCmd};

/// `DisableItems` whitelist (`pkg/cli/cmds/stage.go:9`).
pub const DISABLE_ITEMS: &[&str] = &[
    "coredns",
    "servicelb",
    "traefik",
    "local-storage",
    "metrics-server",
    "runtimes",
];

/// A fatal conflict carrying the k3s-parity message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fatal(pub String);

/// Validate `server` flags against the fatal rules.
///
/// `seen_noop` is the accept-no-op-warn set from the strip filter — needed
/// because `--cluster-reset-restore-path` and `--cluster-reset` are no-op
/// flags (stripped before clap), yet their mutual rule is fatal (Table B).
pub fn validate_server(cmd: &ServerCmd, seen_noop: &[String]) -> Result<(), Fatal> {
    // Rule: --cluster-reset-restore-path requires --cluster-reset
    // (both no-op; detected via the strip set).
    if seen_noop.iter().any(|s| s == "cluster-reset-restore-path")
        && !seen_noop.iter().any(|s| s == "cluster-reset")
    {
        return Err(Fatal(
            "invalid flag use; --cluster-reset required with --cluster-reset-restore-path"
                .to_string(),
        ));
    }

    // Rule: --disable-etcd requires --server
    if cmd.disable_etcd && cmd.shared.server.is_none() {
        return Err(Fatal(
            "invalid flag use; --server is required with --disable-etcd".to_string(),
        ));
    }

    // Rule: --disable-apiserver conflicts --datastore-endpoint
    if cmd.disable_apiserver && cmd.datastore_endpoint.is_some() {
        return Err(Fatal(
            "invalid flag use; cannot use --disable-apiserver with --datastore-endpoint"
                .to_string(),
        ));
    }

    // Rule: --disable-etcd conflicts --datastore-endpoint
    if cmd.disable_etcd && cmd.datastore_endpoint.is_some() {
        return Err(Fatal(
            "invalid flag use; cannot use --disable-etcd with --datastore-endpoint".to_string(),
        ));
    }

    // Rule: unknown --disable token
    for item in &cmd.disable {
        if !DISABLE_ITEMS.contains(&item.as_str()) {
            return Err(Fatal(format!("unknown disable item `{item}`")));
        }
    }

    Ok(())
}

/// Validate `agent` flags: `--token` and `--server` are required (Table B).
pub fn validate_agent(cmd: &AgentCmd) -> Result<(), Fatal> {
    if cmd.shared.token.is_none() {
        return Err(Fatal("--token is required".to_string()));
    }
    if cmd.shared.server.is_none() {
        return Err(Fatal("--server is required".to_string()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::WiredShared;

    fn shared() -> WiredShared {
        WiredShared {
            config: None,
            token: None,
            server: None,
            prefer_bundled_bin: false,
        }
    }

    fn srv(shared: WiredShared) -> ServerCmd {
        ServerCmd {
            shared,
            bind_address: "127.0.0.1".to_string(),
            https_listen_port: 6443,
            disable: vec![],
            disable_etcd: false,
            disable_apiserver: false,
            disable_agent: false,
            disable_controller_manager: false,
            disable_scheduler: false,
            disable_cloud_controller: false,
            disable_kube_proxy: false,
            disable_network_policy: false,
            disable_helm_controller: false,
            datastore_endpoint: None,
            cluster_init: false,
        }
    }

    #[test]
    fn disable_whitelist_accepts_known_tokens() {
        let mut s = srv(shared());
        s.disable = DISABLE_ITEMS.iter().map(|s| s.to_string()).collect();
        assert!(validate_server(&s, &[]).is_ok());
    }

    #[test]
    fn disable_unknown_token_is_fatal() {
        let mut s = srv(shared());
        s.disable = vec!["foo".to_string()];
        let err = validate_server(&s, &[]).unwrap_err();
        assert_eq!(err.0, "unknown disable item `foo`");
    }

    #[test]
    fn disable_unknown_among_known_is_fatal() {
        let mut s = srv(shared());
        s.disable = vec!["coredns".to_string(), "bar".to_string()];
        assert!(validate_server(&s, &[]).is_err());
    }

    #[test]
    fn cluster_reset_restore_path_needs_cluster_reset() {
        let s = srv(shared());
        let seen = vec!["cluster-reset-restore-path".to_string()];
        let err = validate_server(&s, &seen).unwrap_err();
        assert!(err.0.contains("--cluster-reset required"));
    }

    #[test]
    fn cluster_reset_restore_path_ok_with_cluster_reset() {
        let s = srv(shared());
        let seen = vec![
            "cluster-reset-restore-path".to_string(),
            "cluster-reset".to_string(),
        ];
        assert!(validate_server(&s, &seen).is_ok());
    }

    #[test]
    fn disable_etcd_needs_server() {
        let mut s = srv(shared());
        s.disable_etcd = true;
        let err = validate_server(&s, &[]).unwrap_err();
        assert!(err.0.contains("--server is required with --disable-etcd"));
    }

    #[test]
    fn disable_etcd_with_server_ok() {
        let mut sh = shared();
        sh.server = Some("https://x".to_string());
        let mut s = srv(sh);
        s.disable_etcd = true;
        assert!(validate_server(&s, &[]).is_ok());
    }

    #[test]
    fn disable_apiserver_conflicts_datastore_endpoint() {
        let mut s = srv(shared());
        s.disable_apiserver = true;
        s.datastore_endpoint = Some("mysql://x".to_string());
        let err = validate_server(&s, &[]).unwrap_err();
        assert!(err.0.contains("cannot use --disable-apiserver with --datastore-endpoint"));
    }

    #[test]
    fn disable_etcd_conflicts_datastore_endpoint() {
        let mut sh = shared();
        sh.server = Some("https://x".to_string());
        let mut s = srv(sh);
        s.disable_etcd = true;
        s.datastore_endpoint = Some("mysql://x".to_string());
        let err = validate_server(&s, &[]).unwrap_err();
        assert!(err.0.contains("cannot use --disable-etcd with --datastore-endpoint"));
    }

    #[test]
    fn clean_server_validates() {
        let mut s = srv(shared());
        s.disable = vec!["traefik".to_string()];
        assert!(validate_server(&s, &[]).is_ok());
    }

    // ---- agent rules ----

    #[test]
    fn agent_requires_token() {
        let a = AgentCmd { shared: shared() };
        let err = validate_agent(&a).unwrap_err();
        assert_eq!(err.0, "--token is required");
    }

    #[test]
    fn agent_requires_server() {
        let mut sh = shared();
        sh.token = Some("t".to_string());
        let a = AgentCmd { shared: sh };
        let err = validate_agent(&a).unwrap_err();
        assert_eq!(err.0, "--server is required");
    }

    #[test]
    fn agent_with_token_and_server_ok() {
        let mut sh = shared();
        sh.token = Some("t".to_string());
        sh.server = Some("https://x".to_string());
        let a = AgentCmd { shared: sh };
        assert!(validate_agent(&a).is_ok());
    }

    #[test]
    fn disable_items_exact_set() {
        // Guards against drift from the k3s whitelist.
        assert_eq!(
            DISABLE_ITEMS,
            &[
                "coredns",
                "servicelb",
                "traefik",
                "local-storage",
                "metrics-server",
                "runtimes"
            ]
        );
    }

}
