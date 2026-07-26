//! Ingress → [`RouteTable`] compiler (TODO **T5.4**).
//!
//! Turns Kubernetes `networking/v1` [`Ingress`] objects into a flat,
//! specificity-ordered route table the reverse-proxy data plane consults.
//! `host` rules, `pathType` (`Prefix`/`Exact`/`ImplementationSpecific`),
//! per-path backends (`Service` name + numeric/named port) and the optional
//! `spec.defaultBackend` are all honoured. `HTTPRoute`/`Gateway` compile to the
//! same table later.
//!
//! Only `Service` backends are routed (a `resource` backend is skipped — there
//! is nothing to proxy to). A warning is logged, not fatal.

use k8s_openapi::api::networking::v1::{Ingress, IngressBackend, IngressRule};

use crate::route::{HostMatcher, PathMatcher, RouteRule, RouteTable, UpstreamRef};

/// Compile a set of Ingress objects into a single [`RouteTable`].
///
/// Rules are gathered from every Ingress and merged; the result is
/// [`RouteTable::finalise`]d (sorted most-specific first).
pub fn compile_ingress(ingresses: &[Ingress]) -> RouteTable {
    let mut table = RouteTable::new();
    for ing in ingresses {
        let origin = ingress_origin(ing);
        let spec = match ing.spec.as_ref() {
            Some(s) => s,
            None => continue,
        };

        // Default backend (no host/path match).
        if let Some(default) = &spec.default_backend {
            if let Some(up) = backend_upstream(default) {
                table = table.with_default(up);
            }
        }

        // Host rules.
        if let Some(rules) = &spec.rules {
            for rule in rules {
                compile_rule(rule, &origin, &mut table);
            }
        }
    }
    table.finalise()
}

/// Compile one [`IngressRule`] into route rules appended to `table`.
fn compile_rule(rule: &IngressRule, origin: &str, table: &mut RouteTable) {
    let host = rule.host.as_deref().unwrap_or("");
    let host_matcher = HostMatcher::new(host);
    let paths = match rule.http.as_ref() {
        Some(h) => &h.paths,
        None => return,
    };
    for path in paths {
        let path_type = path.path_type.as_str();
        let raw_path = path.path.as_deref().unwrap_or("/");
        let path_matcher = PathMatcher::new(path_type, raw_path);
        let Some(upstream) = backend_upstream(&path.backend) else {
            tracing::warn!(
                target: "init-pro",
                %origin,
                path = raw_path,
                "ingress backend is not a Service; skipping"
            );
            continue;
        };
        table.push(RouteRule {
            host: host_matcher.clone(),
            path: path_matcher,
            upstream,
            origin: origin.to_owned(),
        });
    }
}

/// Extract an [`UpstreamRef`] from an [`IngressBackend`], if it is a Service.
fn backend_upstream(backend: &IngressBackend) -> Option<UpstreamRef> {
    let svc = backend.service.as_ref()?;
    let port = svc.port.as_ref()?;
    let port_ref = match (port.number, port.name.as_deref()) {
        (Some(n), _) => {
            // Clamp the i32 port into u16 (K8s max is 65535).
            crate::route::PortRef::Number(n.clamp(0, 65535) as u16)
        }
        (None, Some(name)) => crate::route::PortRef::Named(name.to_owned()),
        (None, None) => return None,
    };
    Some(UpstreamRef { service: svc.name.clone(), port: port_ref })
}

/// `ingress/<namespace>/<name>` label for tracing.
fn ingress_origin(ing: &Ingress) -> String {
    let ns = ing.metadata.namespace.as_deref().unwrap_or("default");
    let name = ing.metadata.name.as_deref().unwrap_or("<unnamed>");
    format!("ingress/{ns}/{name}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::api::networking::v1::{
        HTTPIngressPath, HTTPIngressRuleValue, IngressBackend, IngressRule,
        IngressServiceBackend, IngressSpec, IngressTLS, ServiceBackendPort,
    };
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;

    fn svc_backend(name: &str, port: i32) -> IngressBackend {
        IngressBackend {
            service: Some(IngressServiceBackend {
                name: name.to_owned(),
                port: Some(ServiceBackendPort { name: None, number: Some(port) }),
            }),
            resource: None,
        }
    }

    fn path(backend: IngressBackend, p: &str, ty: &str) -> HTTPIngressPath {
        HTTPIngressPath { backend, path: Some(p.to_owned()), path_type: ty.to_owned() }
    }

    fn rule(host: Option<&str>, paths: Vec<HTTPIngressPath>) -> IngressRule {
        IngressRule {
            host: host.map(str::to_owned),
            http: Some(HTTPIngressRuleValue { paths }),
        }
    }

    fn ingress(name: &str, spec: IngressSpec) -> Ingress {
        Ingress {
            metadata: ObjectMeta { name: Some(name.to_owned()), ..Default::default() },
            spec: Some(spec),
            status: None,
        }
    }

    #[test]
    fn compiles_host_and_paths() {
        let ing = ingress(
            "edge",
            IngressSpec {
                default_backend: None,
                ingress_class_name: None,
                rules: Some(vec![rule(
                    Some("api.example.com"),
                    vec![
                        path(svc_backend("api", 8080), "/v1", "Prefix"),
                        path(svc_backend("health", 8081), "/health", "Exact"),
                    ],
                )]),
                tls: Some(Vec::<IngressTLS>::new()),
            },
        );
        let table = compile_ingress(&[ing]);
        assert_eq!(table.len(), 2);
        let r = table.lookup("api.example.com", "/v1/users").unwrap();
        assert_eq!(r.upstream.service, "api");
        let r = table.lookup("api.example.com", "/health").unwrap();
        assert_eq!(r.upstream.service, "health");
        assert_eq!(r.path, PathMatcher::new("Exact", "/health"));
    }

    #[test]
    fn default_backend_used_when_no_rule_matches() {
        let ing = ingress(
            "d",
            IngressSpec {
                default_backend: Some(svc_backend("fallback", 80)),
                ingress_class_name: None,
                rules: None,
                tls: None,
            },
        );
        let table = compile_ingress(&[ing]);
        assert_eq!(table.default_upstream().unwrap().service, "fallback");
    }

    #[test]
    fn resource_backend_is_skipped() {
        let mut backend = svc_backend("api", 80);
        backend.service = None; // force non-Service (resource-only)
        let ing = ingress(
            "r",
            IngressSpec {
                default_backend: None,
                ingress_class_name: None,
                rules: Some(vec![rule(Some("h.io"), vec![path(backend, "/", "Prefix")])]),
                tls: None,
            },
        );
        let table = compile_ingress(&[ing]);
        assert!(table.lookup("h.io", "/").is_none());
        assert_eq!(table.len(), 0);
    }

    #[test]
    fn merges_multiple_ingresses_and_orders_specificity() {
        let generic = ingress(
            "g",
            IngressSpec {
                default_backend: None,
                ingress_class_name: None,
                rules: Some(vec![rule(
                    None,
                    vec![path(svc_backend("catch", 80), "/", "Prefix")],
                )]),
                tls: None,
            },
        );
        let specific = ingress(
            "s",
            IngressSpec {
                default_backend: None,
                ingress_class_name: None,
                rules: Some(vec![rule(
                    Some("exact.io"),
                    vec![path(svc_backend("precise", 80), "/x", "Exact")],
                )]),
                tls: None,
            },
        );
        let table = compile_ingress(&[generic, specific]);
        // The exact host + exact path wins despite being declared second.
        let r = table.lookup("exact.io", "/x").unwrap();
        assert_eq!(r.upstream.service, "precise");
    }
}
