//! Programmatic openrusty `Config` assembly (workstream A, decision **Q32**).
//!
//! The Lua era shipped TOML config strings ([`crate::config`], deleted);
//! openrusty exposes public config structs, so the router builds them in
//! memory and applies them hot via `openrusty_server::reload::apply_config`.
//! Everything here is a pure builder + validator call — no I/O, no plugins
//! (the data plane is wasm-free; `PluginsConfig` is an explicit no-op).
//!
//! Upstream naming: `svc/<namespace>/<name>:<port>` — unique per
//! [`UpstreamRef`] (validate() rejects duplicates/empties), stable across
//! reconciles, and readable in openrusty access logs.

use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;

use openrusty_core::config::{
    validate, BalancerKind, Config, FailPolicy, HealthConfig, PeerConfig, PluginsConfig,
    RouteConfig, ServerConfig, UpstreamConfig,
};

use crate::route::{HostMatcher, PathMatcher, PortRef, RouteTable, UpstreamRef};

/// Passive-health defaults mirrored from openrusty's serde defaults; active
/// probing stays OFF (documented deviation, Q32): echo/HTTP backends have no
/// universal probe path, and a non-2xx `/` answer would mark healthy peers
/// down. Passive `max_fails` circuit-breaking still applies.
fn passive_health() -> HealthConfig {
    HealthConfig {
        max_fails: 3,
        fail_window_s: 10,
        fail_timeout_s: 10,
        active: None,
    }
}

/// Explicit plugin no-op: the wasm engine is never engaged (Q32). The dir
/// must still *exist* — `apply_config` validates by scanning it — so point
/// it at the OS temp dir (guaranteed present, contains no `*.wasm`).
fn plugins_noop() -> PluginsConfig {
    PluginsConfig {
        dir: std::env::temp_dir().to_string_lossy().into_owned(),
        order: Vec::new(),
        timeout_ms: 50,
        max_memory_mb: 16,
        on_failure: FailPolicy::FailOpen,
        settings: BTreeMap::<String, HashMap<String, String>>::new(),
    }
}

/// Base config for one listener: HTTP/1.1 + h2c auto-detect, quiet logs.
fn base_config(listen: SocketAddr) -> Config {
    Config {
        server: ServerConfig {
            listen,
            log_level: "warn".to_string(),
            http1_only: false,
        },
        plugins: plugins_noop(),
        upstreams: Vec::new(),
        routes: Vec::new(),
    }
}

/// `svc/<ns>/<name>:<port>` — the openrusty upstream name for a Service port.
fn upstream_name(up: &UpstreamRef) -> String {
    let port = match &up.port {
        PortRef::Number(n) => n.to_string(),
        PortRef::Named(n) => n.clone(),
    };
    format!("svc/{}:{}", up.service, port)
}

/// One Service-port upstream: SWRR balancing (k3s endpoints model) with one
/// retry per request (`retries + 1` attempts, connect/send failures only).
fn upstream_entry(name: String, peers: Vec<SocketAddr>) -> UpstreamConfig {
    UpstreamConfig {
        name,
        balancer: BalancerKind::Swrr,
        retries: 1,
        retry_on_timeout: false,
        connect_timeout_ms: 2000,
        pool_idle_timeout_ms: 60_000,
        peers: peers
            .into_iter()
            .map(|addr| PeerConfig { addr, weight: 1 })
            .collect(),
        health: passive_health(),
    }
}

/// NodePort listener config: one catch-all route onto the Service upstream.
/// An empty peer list yields an empty-but-valid config — the listener still
/// binds and the gate answers the Q28 503 while Endpoints are unresolved.
pub(crate) fn nodeport_config(
    listen: SocketAddr,
    upstream: &UpstreamRef,
    peers: &[SocketAddr],
) -> Config {
    let mut cfg = base_config(listen);
    if peers.is_empty() {
        return cfg;
    }
    let name = upstream_name(upstream);
    cfg.upstreams.push(upstream_entry(name.clone(), peers.to_vec()));
    cfg.routes.push(RouteConfig {
        path_prefix: "/".to_string(),
        host: None,
        upstream: name,
        timeout_ms: 0,
    });
    cfg
}

/// Which parts of a compiled [`RouteTable`] openrusty can express: hosts must
/// be exact-or-any (openrusty matches hosts exactly; `*.suffix` wildcards are
/// a documented Q32 deviation and are dropped with a warning at compile time).
pub(crate) fn expressible(table: &RouteTable) -> RouteTable {
    let mut out = RouteTable::new();
    for rule in table.rules() {
        if matches!(rule.host, HostMatcher::Wildcard(_)) {
            continue;
        }
        out.push(rule.clone());
    }
    if let Some(d) = table.default_upstream() {
        out = out.with_default(d.clone());
    }
    out.finalise()
}

/// Host string for an openrusty route (`None` = host-less). Wildcards are
/// handled by the caller (`expressible` drops them; defensive None here).
fn route_host(rule: &crate::route::RouteRule) -> Option<String> {
    match &rule.host {
        HostMatcher::Exact(h) => Some(h.clone()),
        HostMatcher::Any => None,
        HostMatcher::Wildcard(_) => None,
    }
}

/// The upstream an Exact path falls back to for the `/x/…` extension space:
/// the longest host-less rule matching `p + "/"`, else the default backend.
fn exact_fallback(table: &RouteTable, path: &str) -> Option<UpstreamRef> {
    let probe = format!("{}/", path.trim_end_matches('/'));
    let mut best: Option<(usize, &UpstreamRef)> = None;
    for rule in table.rules() {
        if !matches!(rule.host, HostMatcher::Any) {
            continue;
        }
        let matcher = &rule.path;
        if matcher.matches(&probe) {
            let len = matcher.len();
            if best.map(|(best_len, _)| len >= best_len).unwrap_or(true) {
                best = Some((len, &rule.upstream));
            }
        }
    }
    best.map(|(_, up)| up.clone())
        .or_else(|| table.default_upstream().cloned())
}

/// Ingress gateway config from a compiled [`RouteTable`].
///
/// Mapping (Q32; parity with the Lua-era [`crate::route::RouteTable`] where
/// openrusty allows it):
/// - exact hosts become `routes.host`, host-less rules become host-less
///   routes (openrusty's host-class-then-`or_else` matching reproduces the
///   old "specific host first, host-less fallback" semantics);
/// - `Exact` paths compile to the prefix `p` plus a synthetic `p + "/"`
///   fallback route (the extension space belongs to the host-less class, as
///   in the old table). Documented deviation: `/x`-exact ALSO matches
///   `/xyz`-style extensions (openrusty prefixes have no separator boundary);
/// - empty-peer upstreams are omitted entirely — the gate 503s them;
/// - the default backend becomes the host-less `"/"` catch-all.
pub(crate) fn ingress_config(
    listen: SocketAddr,
    table: &RouteTable,
    peers_of: &dyn Fn(&UpstreamRef) -> Vec<SocketAddr>,
) -> Config {
    let mut cfg = base_config(listen);
    let mut emitted: Vec<String> = Vec::new();
    let mut ensure_upstream = |cfg: &mut Config, up: &UpstreamRef| -> Option<String> {
        let peers = peers_of(up);
        if peers.is_empty() {
            return None;
        }
        let name = upstream_name(up);
        if !emitted.contains(&name) {
            cfg.upstreams.push(upstream_entry(name.clone(), peers));
            emitted.push(name.clone());
        }
        Some(name)
    };
    let push = |cfg: &mut Config, host: Option<String>, prefix: String, upstream: String| {
        cfg.routes.push(RouteConfig {
            path_prefix: prefix,
            host,
            upstream,
            timeout_ms: 0,
        });
    };

    for rule in table.rules() {
        // Wildcard-host rules were dropped from `expressible`; defensively
        // skip any that still arrive (openrusty would over-match them).
        if matches!(rule.host, HostMatcher::Wildcard(_)) {
            tracing::warn!(target: "init-pro", "wildcard Ingress host dropped (openrusty matches hosts exactly)");
            continue;
        }
        let host = route_host(rule);
        let Some(name) = ensure_upstream(&mut cfg, &rule.upstream) else {
            continue; // no live peers -> gate 503s this subtree
        };
        match &rule.path {
            PathMatcher::Exact(p) => {
                push(&mut cfg, host.clone(), p.clone(), name);
                if let Some(fallback) = exact_fallback(table, p) {
                    if let Some(fb_name) = ensure_upstream(&mut cfg, &fallback) {
                        push(&mut cfg, host.clone(), format!("{}/", p), fb_name);
                    }
                }
            }
            PathMatcher::Prefix(p) => push(&mut cfg, host.clone(), p.clone(), name),
        }
    }

    // Default backend -> host-less "/" catch-all (openrusty's host-less
    // class is only consulted after host-specific classes).
    if let Some(default) = table.default_upstream() {
        if let Some(name) = ensure_upstream(&mut cfg, default) {
            push(&mut cfg, None, "/".to_string(), name);
        }
    }

    if let Err(e) = validate(&cfg) {
        tracing::warn!(target: "init-pro", error = %e, "generated ingress config failed validation");
    }
    cfg
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::route::RouteRule;

    fn rule(host: HostMatcher, path: PathMatcher, svc: &str) -> RouteRule {
        RouteRule {
            host,
            path,
            upstream: UpstreamRef::port(svc, 80),
            origin: format!("test/{svc}"),
        }
    }

    fn table(rules: Vec<RouteRule>, default: Option<&str>) -> RouteTable {
        let mut t = RouteTable::new();
        for r in rules {
            t.push(r);
        }
        if let Some(d) = default {
            t = t.with_default(UpstreamRef::port(d, 80));
        }
        t.finalise()
    }

    fn peers(_up: &UpstreamRef) -> Vec<SocketAddr> {
        vec!["127.0.0.1:9001".parse().unwrap()]
    }

    #[test]
    fn nodeport_config_is_valid_and_catch_all() {
        let listen: SocketAddr = "127.0.0.1:30080".parse().unwrap();
        let up = UpstreamRef::port("default/web", 80);
        let cfg = nodeport_config(listen, &up, &peers(&up));
        validate(&cfg).expect("nodeport config valid");
        assert_eq!(cfg.upstreams.len(), 1);
        assert_eq!(cfg.upstreams[0].name, "svc/default/web:80");
        assert_eq!(cfg.upstreams[0].peers.len(), 1);
        assert_eq!(cfg.routes.len(), 1);
        assert_eq!(cfg.routes[0].path_prefix, "/");
        assert!(cfg.routes[0].host.is_none());
        assert_eq!(cfg.plugins.dir, std::env::temp_dir().to_string_lossy());
    }

    #[test]
    fn nodeport_config_empty_peers_stays_valid_and_empty() {
        let listen: SocketAddr = "127.0.0.1:30080".parse().unwrap();
        let cfg = nodeport_config(listen, &UpstreamRef::port("default/web", 80), &[]);
        validate(&cfg).expect("empty config valid");
        assert!(cfg.upstreams.is_empty() && cfg.routes.is_empty());
    }

    #[test]
    fn ingress_config_emits_host_routes_and_default_backend() {
        let listen: SocketAddr = "127.0.0.1:30080".parse().unwrap();
        let t = table(
            vec![
                rule(HostMatcher::new(""), PathMatcher::new("Prefix", "/"), "catch"),
                rule(HostMatcher::new("api.io"), PathMatcher::new("Prefix", "/v1"), "api"),
                rule(HostMatcher::new("*.io"), PathMatcher::new("Prefix", "/w"), "wild"),
            ],
            Some("fallback"),
        );
        let cfg = ingress_config(listen, &expressible(&t), &peers);
        validate(&cfg).expect("ingress config valid");
        // Wildcard rule dropped; 2 host-class rules + default backend route.
        assert_eq!(cfg.upstreams.len(), 3, "api + catch + fallback");
        let hosts: Vec<Option<&String>> = cfg.routes.iter().map(|r| r.host.as_ref()).collect();
        assert!(hosts.contains(&Some(&"api.io".to_string())));
        assert!(hosts.contains(&None));
        // The catch-all is last (default backend -> "/").
        let last = cfg.routes.last().unwrap();
        assert_eq!(last.path_prefix, "/");
        assert_eq!(last.upstream, "svc/fallback:80");
    }

    #[test]
    fn exact_path_gets_extension_fallback_route() {
        let listen: SocketAddr = "127.0.0.1:30081".parse().unwrap();
        let t = table(
            vec![
                rule(HostMatcher::new("api.io"), PathMatcher::new("Exact", "/x"), "exact"),
                rule(HostMatcher::new(""), PathMatcher::new("Prefix", "/x"), "broad"),
            ],
            None,
        );
        let cfg = ingress_config(listen, &expressible(&t), &peers);
        validate(&cfg).expect("valid");
        let prefixes: Vec<&str> = cfg.routes.iter().map(|r| r.path_prefix.as_str()).collect();
        assert!(prefixes.contains(&"/x"), "exact route emitted");
        assert!(prefixes.contains(&"/x/"), "extension fallback emitted");
        let fallback = cfg.routes.iter().find(|r| r.path_prefix == "/x/").unwrap();
        assert_eq!(fallback.upstream, "svc/broad:80");
    }

    #[test]
    fn empty_peer_upstreams_are_omitted() {
        let listen: SocketAddr = "127.0.0.1:30082".parse().unwrap();
        let t = table(
            vec![rule(HostMatcher::new("api.io"), PathMatcher::new("Prefix", "/v1"), "api")],
            None,
        );
        let cfg = ingress_config(listen, &t, &|_| Vec::new());
        assert!(cfg.upstreams.is_empty());
        assert!(cfg.routes.is_empty());
        validate(&cfg).expect("still valid");
    }

    /// Probe: the generated nodeport config actually ROUTES through the
    /// openrusty runtime (app::router oneshot, no listener needed).
    #[tokio::test]
    async fn generated_config_routes_through_openrusty() {
        use tower::ServiceExt as _;

        let listen: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
        let up = UpstreamRef::port("default/web", 80);
        let peers = vec!["127.0.0.1:39999".parse().unwrap()];
        let cfg = nodeport_config(listen, &up, &peers);
        let app = openrusty_server::state::from_config(cfg, std::path::PathBuf::from("probe"))
            .expect("bootstrap");
        let svc = openrusty_server::app::router(app);
        let req = axum::extract::Request::builder()
            .uri("/")
            .header("host", "test")
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = svc.oneshot(req).await.expect("oneshot");
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 16)
            .await
            .expect("body");
        // No live peer at :39999 -> connect failure -> openrusty 502/504,
        // but NEVER 404 (a 404 would mean the route did not match).
        assert!(!bytes.windows(3).any(|w| w == b"404"), "route did not match");
    }

    #[test]
    fn expressible_drops_wildcard_hosts() {
        let t = table(
            vec![
                rule(HostMatcher::new("*.io"), PathMatcher::new("Prefix", "/"), "wild"),
                rule(HostMatcher::new("a.io"), PathMatcher::new("Prefix", "/"), "exact"),
            ],
            None,
        );
        let kept = expressible(&t);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept.rules()[0].upstream.service, "exact");
    }
}
