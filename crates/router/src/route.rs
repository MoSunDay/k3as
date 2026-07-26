//! Compiled HTTP route table: host × path → upstream (TODO **T5.4**).
//!
//! Built by the Ingress compiler ([`crate::ingress`]) and consulted by the
//! reverse-proxy data plane ([`crate::proxy`]). Matching follows Kubernetes
//! `Ingress` semantics: a request matches the **first** rule (in specificity
//! order) whose host and path both match. Hosts may be exact (`foo.bar`) or
//! wildcard (`*.bar`); paths are `Prefix` (element-wise), `Exact`, or
//! `ImplementationSpecific` (treated as `Prefix` for v1).

/// A compiled upstream reference: the Service to forward to + which port.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct UpstreamRef {
    /// The Kubernetes `Service` name backing this route.
    pub service: String,
    /// The Service port (numeric or named; named resolved by the resolver).
    pub port: PortRef,
}

impl UpstreamRef {
    /// Convenience constructor for a numeric port.
    pub fn port(service: impl Into<String>, port: u16) -> Self {
        Self { service: service.into(), port: PortRef::Number(port) }
    }
}

/// A Service port: numeric or named (Scope A resolves named via the resolver).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum PortRef {
    /// A numeric port (e.g. `80`).
    Number(u16),
    /// A named port (resolved by [`crate::balancer::UpstreamResolver`]).
    Named(String),
}

/// How a rule's host header is matched.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostMatcher {
    /// Match any host (an empty Ingress `host`).
    Any,
    /// Exact host name (`foo.bar`).
    Exact(String),
    /// Wildcard `*.suffix` — matches exactly one more label before the suffix.
    Wildcard(String),
}

impl HostMatcher {
    /// Parse an Ingress host string into a matcher.
    pub fn new(host: &str) -> Self {
        if host.is_empty() {
            HostMatcher::Any
        } else if let Some(rest) = host.strip_prefix("*.") {
            HostMatcher::Wildcard(rest.to_ascii_lowercase())
        } else {
            HostMatcher::Exact(host.to_ascii_lowercase())
        }
    }

    /// True if `req_host` (port-stripped) satisfies this matcher.
    pub fn matches(&self, req_host: &str) -> bool {
        let host = host_without_port(req_host).to_ascii_lowercase();
        match self {
            HostMatcher::Any => true,
            HostMatcher::Exact(h) => host == *h,
            // K8s: `*.foo.com` matches a single extra label, so `bar.foo.com`
            // matches but `a.b.foo.com` and `foo.com` do not.
            HostMatcher::Wildcard(suffix) => {
                // `*.example.com` matches exactly one extra label: the char
                // before the suffix is '.', and the single label before it
                // contains no further dots (so `a.example.com` matches but
                // `a.b.example.com` does not).
                if !host.ends_with(suffix) || host.len() <= suffix.len() {
                    return false;
                }
                let sep = host.len() - suffix.len() - 1;
                if host.as_bytes()[sep] != b'.' {
                    return false;
                }
                let label = &host[..sep];
                !label.is_empty() && !label.contains('.')
            }
        }
    }

    fn rank(&self) -> u8 {
        match self {
            HostMatcher::Exact(_) => 3,
            HostMatcher::Wildcard(_) => 2,
            HostMatcher::Any => 1,
        }
    }
}

/// How a rule's path is matched.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PathMatcher {
    /// Element-wise prefix (K8s `Prefix`): `/foo` matches `/foo` and
    /// `/foo/bar` but **not** `/foobar`.
    Prefix(String),
    /// Exact path match.
    Exact(String),
}

impl PathMatcher {
    /// Build from an Ingress `pathType` + path string.
    pub fn new(path_type: &str, path: &str) -> Self {
        match path_type {
            "Exact" => PathMatcher::Exact(path.to_owned()),
            // ImplementationSpecific is treated as Prefix (the common default).
            _ => PathMatcher::Prefix(if path.is_empty() { "/" } else { path }.to_owned()),
        }
    }

    /// True if `req_path` satisfies this matcher (path only, no query).
    pub fn matches(&self, req_path: &str) -> bool {
        match self {
            PathMatcher::Exact(p) => req_path == p,
            PathMatcher::Prefix(p) => prefix_match(p, req_path),
        }
    }

    /// Longer = more specific (used for ordering).
    fn len(&self) -> usize {
        match self {
            PathMatcher::Prefix(p) | PathMatcher::Exact(p) => p.len(),
        }
    }

    fn rank(&self) -> u8 {
        match self {
            PathMatcher::Exact(_) => 2,
            PathMatcher::Prefix(_) => 1,
        }
    }
}

/// Element-wise prefix match (K8s `Prefix` semantics).
fn prefix_match(rule: &str, req: &str) -> bool {
    let rule = rule.trim_end_matches('/');
    if rule.is_empty() {
        return true; // "/" matches everything.
    }
    if req == rule {
        return true;
    }
    // req must start with rule and the next byte must be a path separator.
    req.len() > rule.len()
        && req.starts_with(rule)
        && req.as_bytes()[rule.len()] == b'/'
}

/// One compiled routing rule.
#[derive(Clone, Debug)]
pub struct RouteRule {
    /// Host header matcher.
    pub host: HostMatcher,
    /// Path matcher.
    pub path: PathMatcher,
    /// The upstream this rule routes to.
    pub upstream: UpstreamRef,
    /// Human-readable origin (e.g. `ingress/<ns>/<name>`) for tracing.
    pub origin: String,
}

/// A specificity rank used to order rules (higher wins on ties).
fn specificity(rule: &RouteRule) -> (u8, u8, usize) {
    (rule.host.rank(), rule.path.rank(), rule.path.len())
}

/// An ordered set of route rules, most specific first, plus an optional default
/// backend (used when no rule matches).
#[derive(Clone, Debug, Default)]
pub struct RouteTable {
    rules: Vec<RouteRule>,
    default: Option<UpstreamRef>,
    /// Monotonic generation stamp (T5.4 Scope B / T5.5): bumped each time the
    /// table is swapped into the live [`crate::config::RouteStore`]. Lets the
    /// data plane log/route on a known generation and tests assert a swap
    /// occurred without restarting the process.
    pub(crate) generation: u64,
}

impl RouteTable {
    /// Empty table.
    pub fn new() -> Self {
        Self::default()
    }

    /// The current generation stamp.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Set the default backend (K8s `spec.defaultBackend`).
    pub fn with_default(mut self, upstream: UpstreamRef) -> Self {
        self.default = Some(upstream);
        self
    }

    /// Add a rule (order is finalised by [`Self::finalise`]).
    pub fn push(&mut self, rule: RouteRule) {
        self.rules.push(rule);
    }

    /// Sort rules by descending specificity (longest, most exact first).
    pub fn finalise(mut self) -> Self {
        // Descending specificity: most-exact host + longest path first.
        self.rules.sort_by_key(|r| std::cmp::Reverse(specificity(r)));
        self
    }

    /// Find the first rule matching `(host, path)`, most specific first.
    pub fn lookup(&self, host: &str, path: &str) -> Option<&RouteRule> {
        self.rules.iter().find(|r| r.host.matches(host) && r.path.matches(path))
    }

    /// The default backend, if any.
    pub fn default_upstream(&self) -> Option<&UpstreamRef> {
        self.default.as_ref()
    }

    /// All rules (in their finalised order).
    pub fn rules(&self) -> &[RouteRule] {
        &self.rules
    }

    /// Number of rules.
    pub fn len(&self) -> usize {
        self.rules.len()
    }

    /// Whether the table has no rules and no default.
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty() && self.default.is_none()
    }
}

/// Strip the `:port` suffix from a Host header value, if present.
fn host_without_port(host: &str) -> &str {
    // IPv6 hosts are bracketed; leave them intact (not a Scope A concern).
    if host.starts_with('[') {
        return host;
    }
    match host.rfind(':') {
        Some(idx) if !host[..idx].contains(':') => &host[..idx],
        _ => host,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_matching_element_wise() {
        let m = PathMatcher::new("Prefix", "/foo");
        assert!(m.matches("/foo"));
        assert!(m.matches("/foo/"));
        assert!(m.matches("/foo/bar"));
        assert!(!m.matches("/foobar"));
        assert!(!m.matches("/fo"));
    }

    #[test]
    fn prefix_root_matches_all() {
        let m = PathMatcher::new("Prefix", "/");
        assert!(m.matches("/"));
        assert!(m.matches("/anything/deep"));
    }

    #[test]
    fn wildcard_host_single_label() {
        let m = HostMatcher::new("*.example.com");
        assert!(m.matches("a.example.com"));
        assert!(!m.matches("a.b.example.com"));
        assert!(!m.matches("example.com"));
    }

    #[test]
    fn exact_host_port_stripped() {
        let m = HostMatcher::new("api.io");
        assert!(m.matches("api.io:8080"));
        assert!(m.matches("API.IO"));
    }

    #[test]
    fn table_returns_most_specific_first() {
        let mut t = RouteTable::new();
        t.push(RouteRule {
            host: HostMatcher::Any,
            path: PathMatcher::new("Prefix", "/"),
            upstream: UpstreamRef::port("catch", 80),
            origin: "default".into(),
        });
        t.push(RouteRule {
            host: HostMatcher::Exact("api.io".into()),
            path: PathMatcher::new("Exact", "/v1/health"),
            upstream: UpstreamRef::port("health", 80),
            origin: "exact".into(),
        });
        let t = t.finalise();
        let r = t.lookup("api.io", "/v1/health").unwrap();
        assert_eq!(r.upstream.service, "health");
        let r = t.lookup("api.io", "/v1/other").unwrap();
        assert_eq!(r.upstream.service, "catch");
    }
}
