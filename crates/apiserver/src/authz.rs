//! RBAC authorization middleware (TODO **T1.3b**, decisions **Q47/Q59**).
//!
//! Supported-subset evaluator: resolves the caller's [`crate::authn`]
//! identity through `ClusterRoleBinding`s (cluster-wide) and
//! `RoleBinding`s (binding namespace) into `ClusterRole`/`Role` rule
//! sets (`api::rbac::PolicyRule`) and allows when any rule grants the
//! request's verb × group × resource (× name). Non-resource endpoints
//! (discovery, `/readyz`, `/version`) are allowed for ANY authenticated
//! user — the subset has no `nonResourceURL` matching (upstream's
//! `system:public-info-viewer` posture collapsed to "authenticated").
//!
//! In-process control-plane traffic (controllers/scheduler/router over the
//! shared storage `Arc`, Q19) never crosses this middleware — only the
//! HTTP plane does. Authenticated + no matching rule -> 403 with a
//! Kubernetes-style `Status` body.

use std::sync::Arc;

use axum::extract::State;
use axum::http::{Method, Request, StatusCode, Uri};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde_json::json;
use storage::{Key, KeyPrefix, StorageBackend};

use api::rbac::{ClusterRoleBinding, PolicyRule, RoleBinding};

use crate::authn::Identity;
use crate::state::AppState;

pub(crate) const RBAC_GROUP: &str = "rbac.authorization.k8s.io";

/// The request's resource attributes (upstream `authorization.Attributes`
/// subset; subresources ride along but rules match the base resource).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attrs {
    pub verb: String,
    pub group: String,
    pub resource: String,
    pub subresource: Option<String>,
    pub namespace: Option<String>,
    pub name: Option<String>,
}

impl Attrs {
    /// The `PolicyRule` view of these attrs (base resource; subresources
    /// are covered by their base resource in this subset).
    fn grant_key(&self) -> (&str, &str, Option<&str>) {
        (
            self.group.as_str(),
            self.resource.as_str(),
            self.name.as_deref(),
        )
    }
}

/// One parsed request target: either a resource request (authz applies)
/// or a non-resource path (authenticated-allow).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Resource(Attrs),
    NonResource(String),
}

/// Map method + path to a [`Target`]. Pure; exercised by unit tests.
pub fn parse_target(method: &Method, uri: &Uri) -> Target {
    let path = uri.path().trim_end_matches('/');
    let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let watch = uri
        .query()
        .map(|q| q.split('&').any(|p| p == "watch=1"))
        .unwrap_or(false);
    let verb = |name: Option<&str>| -> String {
        // `?watch=1` turns any GET (list or single-object) into "watch".
        match (method, watch, name.is_some()) {
            (&Method::GET, true, _) => "watch".into(),
            (&Method::GET, false, true) => "get".into(),
            (&Method::GET, false, false) => "list".into(),
            (&Method::POST, _, _) => "create".into(),
            (&Method::PUT, _, _) => "update".into(),
            (&Method::PATCH, _, _) => "patch".into(),
            (&Method::DELETE, _, _) => "delete".into(),
            _ => "other".into(),
        }
    };
    // core group: /api/v1/...
    let core_rest = match segs.as_slice() {
        ["api", "v1", rest @ ..] => Some(("", rest)),
        _ => None,
    };
    // named groups: /apis/<group>/<version>/...
    let group_rest = match segs.as_slice() {
        ["apis", g, _v, rest @ ..] => Some((*g, rest)),
        _ => None,
    };
    let (group, rest) = match core_rest.or(group_rest) {
        Some((g, rest)) if !rest.is_empty() => (g, rest),
        _ => return Target::NonResource(path.to_string()),
    };
    // namespaced disambiguation (upstream request-info rule):
    // [..., "namespaces"] / [..., "namespaces", name] => the namespaces
    // resource itself; [..., "namespaces", ns, resource, ...] => scoped.
    let (namespace, rest): (Option<String>, &[&str]) = match rest {
        ["namespaces"] => (None, rest),
        ["namespaces", _name] => (None, rest),
        ["namespaces", ns, sub @ ..] if !sub.is_empty() => (Some((*ns).to_string()), sub),
        _ => (None, rest),
    };
    let attrs = match rest {
        [resource] => Attrs {
            verb: verb(None),
            group: group.to_string(),
            resource: resource.to_string(),
            subresource: None,
            namespace: namespace.clone(),
            name: None,
        },
        [resource, name] => Attrs {
            verb: verb(Some(name)),
            group: group.to_string(),
            resource: resource.to_string(),
            subresource: None,
            namespace: namespace.clone(),
            name: Some((*name).to_string()),
        },
        [resource, name, sub] => Attrs {
            verb: verb(Some(name)),
            group: group.to_string(),
            resource: resource.to_string(),
            subresource: Some((*sub).to_string()),
            namespace: namespace.clone(),
            name: Some((*name).to_string()),
        },
        _ => return Target::NonResource(path.to_string()),
    };
    Target::Resource(attrs)
}

/// Kubernetes-style 403 body.
fn forbidden(identity: &Identity, attrs: &Attrs) -> Response {
    let msg = format!(
        "{} is forbidden: User \"{}\" cannot {} resource \"{}\" in API group \"{}\"{}",
        attrs.resource,
        identity.user,
        attrs.verb,
        attrs.resource,
        attrs.group,
        match (&attrs.namespace, &attrs.name) {
            (Some(ns), Some(n)) => format!(" in the namespace \"{ns}\" (name \"{n}\")"),
            (Some(ns), None) => format!(" in the namespace \"{ns}\""),
            _ => String::new(),
        }
    );
    let body = json!({
        "kind": "Status",
        "apiVersion": "v1",
        "metadata": {},
        "status": "Failure",
        "message": msg,
        "reason": "Forbidden",
        "details": {},
        "code": 403,
    });
    (StatusCode::FORBIDDEN, axum::Json(body)).into_response()
}

/// Pure rule-set check: does any `rules` entry grant `attrs`?
fn rules_grant(rules: &[PolicyRule], attrs: &Attrs) -> bool {
    let (group, resource, name) = attrs.grant_key();
    rules
        .iter()
        .any(|r| r.grants(&attrs.verb, group, resource, name))
}

/// List + decode objects under one rbac resource (all namespaces when
/// `ns` is None).
async fn list_objects<T: serde::de::DeserializeOwned>(
    store: &Arc<dyn StorageBackend>,
    resource: &str,
    ns: Option<&str>,
) -> Vec<T> {
    store
        .list(&KeyPrefix::new(
            RBAC_GROUP,
            resource,
            ns.map(|s| s.to_string()),
        ))
        .await
        .unwrap_or_default()
        .into_iter()
        .filter_map(|e| serde_json::from_value(e.value).ok())
        .collect()
}

/// The authorization decision for one request. Namespace rule: rules from
/// a RoleBinding/Role pair apply only when the request carries the SAME
/// namespace (cluster-scoped requests use ClusterRoleBinding chains only).
pub async fn authorize(
    store: &Arc<dyn StorageBackend>,
    identity: &Identity,
    attrs: &Attrs,
) -> bool {
    // ClusterRoleBinding -> ClusterRole (cluster-wide).
    let crbs: Vec<ClusterRoleBinding> = list_objects(store, "clusterrolebindings", None).await;
    for crb in &crbs {
        if !crb
            .subjects
            .iter()
            .any(|s| s.matches(&identity.user, &identity.groups))
        {
            continue;
        }
        if crb.role_ref.kind != "ClusterRole" {
            continue;
        }
        if let Some(v) = store
            .get(&Key::new(
                RBAC_GROUP,
                "clusterroles",
                "",
                &crb.role_ref.name,
            ))
            .await
            .ok()
            .flatten()
            .and_then(|e| serde_json::from_value::<api::rbac::ClusterRole>(e.value).ok())
        {
            if rules_grant(&v.rules, attrs) {
                return true;
            }
        }
    }
    // RoleBinding (namespace-scoped) -> Role/ClusterRole.
    let rbs: Vec<RoleBinding> = list_objects(store, "rolebindings", None).await;
    for rb in &rbs {
        if !rb
            .subjects
            .iter()
            .any(|s| s.matches(&identity.user, &identity.groups))
        {
            continue;
        }
        let binding_ns = rb.metadata.namespace.clone().unwrap_or_default();
        let scope_ok = match &attrs.namespace {
            Some(req_ns) => req_ns == &binding_ns,
            None => false, // cluster-scoped request: ClusterRoleBindings only
        };
        if !scope_ok {
            continue;
        }
        let role = match rb.role_ref.kind.as_str() {
            "Role" => store
                .get(&Key::new(
                    RBAC_GROUP,
                    "roles",
                    &binding_ns,
                    &rb.role_ref.name,
                ))
                .await
                .ok()
                .flatten()
                .and_then(|e| serde_json::from_value::<api::rbac::Role>(e.value).ok())
                .map(|r| r.rules),
            "ClusterRole" => store
                .get(&Key::new(RBAC_GROUP, "clusterroles", "", &rb.role_ref.name))
                .await
                .ok()
                .flatten()
                .and_then(|e| serde_json::from_value::<api::rbac::ClusterRole>(e.value).ok())
                .map(|r| r.rules),
            _ => None,
        };
        if let Some(rules) = role {
            if rules_grant(&rules, attrs) {
                return true;
            }
        }
    }
    false
}

/// The authz middleware: runs AFTER [`crate::authn`] (identity must be in
/// the extensions). Non-resource targets pass; resource targets must
/// match at least one rule.
pub(crate) async fn middleware(
    State(state): State<AppState>,
    req: Request<axum::body::Body>,
    next: Next,
) -> Response {
    let identity = match req.extensions().get::<Identity>() {
        Some(i) => i.clone(),
        None => return next.run(req).await, // authn not armed
    };
    let target = parse_target(req.method(), req.uri());
    match target {
        Target::NonResource(_) => next.run(req).await,
        Target::Resource(attrs) => {
            if authorize(&state.store, &identity, &attrs).await {
                next.run(req).await
            } else {
                forbidden(&identity, &attrs)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uri(p: &str) -> Uri {
        p.parse().unwrap()
    }

    #[test]
    fn parses_core_namespaced_paths() {
        let a = parse_target(&Method::GET, &uri("/api/v1/namespaces/default/configmaps"));
        match a {
            Target::Resource(a) => {
                assert_eq!(a.verb, "list");
                assert_eq!(a.resource, "configmaps");
                assert_eq!(a.namespace.as_deref(), Some("default"));
                assert_eq!(a.name, None);
            }
            _ => panic!("resource"),
        }
        let a = parse_target(
            &Method::GET,
            &uri("/api/v1/namespaces/default/configmaps/cm1?watch=1"),
        );
        match a {
            Target::Resource(a) => {
                assert_eq!(a.verb, "watch");
                assert_eq!(a.name.as_deref(), Some("cm1"));
            }
            _ => panic!("resource"),
        }
    }

    #[test]
    fn parses_grouped_and_subresources() {
        let a = parse_target(
            &Method::POST,
            &uri("/apis/apps/v1/namespaces/default/deployments"),
        );
        match a {
            Target::Resource(a) => {
                assert_eq!(a.verb, "create");
                assert_eq!(a.group, "apps");
                assert_eq!(a.resource, "deployments");
            }
            _ => panic!("resource"),
        }
        let a = parse_target(&Method::GET, &uri("/api/v1/namespaces/default/pods/p1/log"));
        match a {
            Target::Resource(a) => {
                assert_eq!(a.resource, "pods");
                assert_eq!(a.subresource.as_deref(), Some("log"));
                assert_eq!(a.name.as_deref(), Some("p1"));
            }
            _ => panic!("resource"),
        }
        let a = parse_target(&Method::PUT, &uri("/api/v1/nodes/n1/status"));
        match a {
            Target::Resource(a) => {
                assert_eq!(a.resource, "nodes");
                assert_eq!(a.verb, "update");
                assert_eq!(a.namespace, None);
            }
            _ => panic!("resource"),
        }
    }

    #[test]
    fn namespaces_item_is_the_resource() {
        let a = parse_target(&Method::GET, &uri("/api/v1/namespaces/default"));
        match a {
            Target::Resource(a) => {
                assert_eq!(a.resource, "namespaces");
                assert_eq!(a.name.as_deref(), Some("default"));
                assert_eq!(a.namespace, None);
            }
            _ => panic!("resource"),
        }
        let a = parse_target(&Method::GET, &uri("/api/v1/namespaces"));
        match a {
            Target::Resource(a) => {
                assert_eq!(a.verb, "list");
                assert_eq!(a.resource, "namespaces");
            }
            _ => panic!("resource"),
        }
    }

    #[test]
    fn discovery_is_non_resource() {
        for p in [
            "/",
            "/api",
            "/apis",
            "/api/v1",
            "/apis/apps/v1",
            "/readyz",
            "/version",
        ] {
            assert!(
                matches!(parse_target(&Method::GET, &uri(p)), Target::NonResource(_)),
                "{p} non-resource"
            );
        }
    }
}
