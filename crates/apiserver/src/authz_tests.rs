//! Unit tests for the pure request-info parsing in `authz.rs`
//! (target extraction from method + URI) — split out of `authz.rs`
//! in Sprint 51 to keep that file under the 400-line cap (the
//! `pod_io_tests` precedent).

use axum::http::{Method, Uri};

use crate::authz::*;

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
