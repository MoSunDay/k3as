//! Unit tests for the Endpoints/Services fold + resolution (Sprint 18 /
//! **S4**, Q28). Split beside [`super`] to keep `endpoints.rs` under the
//! 400-line new-file ceiling while remaining an in-crate unit-test module.

use super::*;
use serde_json::json;

fn entry(key: &str, value: Value) -> StoredEntry {
    StoredEntry {
        key: key.into(),
        value,
        create_revision: 1,
        mod_revision: 1,
        version: 1,
    }
}

fn svc(name: &str, ns: &str, spec: Value) -> Value {
    json!({"apiVersion": "v1", "kind": "Service",
           "metadata": {"name": name, "namespace": ns}, "spec": spec})
}

fn ep(name: &str, ns: &str, subsets: Value) -> Value {
    json!({"apiVersion": "v1", "kind": "Endpoints",
           "metadata": {"name": name, "namespace": ns}, "subsets": subsets})
}

fn state_from(spec: Value, eps: Value) -> ResolverState {
    let mut st = ResolverState::default();
    let s = parse_service(&svc("web", "default", spec)).unwrap();
    let e = parse_endpoints(&ep("web", "default", eps)).unwrap();
    st.services.insert("default/web".into(), s);
    st.endpoints.insert("default/web".into(), e);
    st
}

fn web_state(target: Value, ep_port: Value) -> ResolverState {
    state_from(
        json!({"type": "NodePort", "ports": [
            {"name": "http", "port": 80, "targetPort": target, "nodePort": 30080}]}),
        json!([{"addresses": [{"ip": "127.0.0.1"}], "ports": [ep_port]}]),
    )
}

fn sock(addr: &str) -> SocketAddr {
    addr.parse().unwrap()
}

#[test]
fn fold_list_replace_and_watch_events() {
    let mut st = ResolverState::default();
    let spec = json!({"type": "NodePort", "ports": [
        {"name": "http", "port": 80, "targetPort": 8080, "nodePort": 30080}]});
    let eps = json!([{"addresses": [{"ip": "10.0.0.5"}],
                      "ports": [{"name": "http", "port": 8080}]}]);
    replace_all(
        &mut st,
        Resource::Services,
        &[entry(
            "/registry/services/default/web",
            svc("web", "default", spec),
        )],
    );
    assert_eq!(st.services["default/web"].ports[0].node_port, Some(30080));
    fold_event(
        &mut st,
        Resource::Endpoints,
        &WatchEvent::Put(Arc::new(entry(
            "/registry/endpoints/default/web",
            ep("web", "default", eps),
        ))),
    );
    assert_eq!(
        st.endpoints["default/web"].subsets[0].addresses,
        vec!["10.0.0.5"]
    );
    fold_event(
        &mut st,
        Resource::Services,
        &WatchEvent::Delete {
            key: "/registry/services/default/web".into(),
            mod_revision: 2,
            prev: None,
        },
    );
    assert!(st.services.is_empty());
}
#[test]
fn parse_skips_not_ready_addresses_and_malformed_docs() {
    let v = ep(
        "x",
        "default",
        json!([{"addresses": [{"ip": "10.0.0.1"}],
        "notReadyAddresses": [{"ip": "10.0.0.2"}], "ports": [{"port": 8080}]}]),
    );
    assert_eq!(
        parse_endpoints(&v).unwrap().subsets[0].addresses,
        vec!["10.0.0.1"]
    );
    assert!(parse_service(&json!({"spec": {}})).is_none());
    assert!(parse_service(&json!({"metadata": {"name": "n"}})).is_none());
    assert!(parse_endpoints(&json!({"kind": "Endpoints"})).is_none());
}
#[test]
fn resolve_numeric_target_port() {
    let st = web_state(json!(8080), json!({"name": "http", "port": 8080}));
    let peers = resolve_with_state(&st, &UpstreamRef::port("default/web", 80));
    assert_eq!(peers, vec![sock("127.0.0.1:8080")]);
}
#[test]
fn resolve_named_service_port() {
    let st = web_state(json!(9000), json!({"name": "http", "port": 9000}));
    let up = UpstreamRef {
        service: "default/web".into(),
        port: PortRef::Named("http".into()),
    };
    assert_eq!(resolve_with_state(&st, &up), vec![sock("127.0.0.1:9000")]);
}
#[test]
fn resolve_identity_when_target_port_absent() {
    let st = state_from(
        json!({"type": "NodePort", "ports": [{"port": 80, "nodePort": 30080}]}),
        json!([{"addresses": [{"ip": "10.1.1.1"}], "ports": [{"port": 80}]}]),
    );
    let peers = resolve_with_state(&st, &UpstreamRef::port("default/web", 80));
    assert_eq!(peers, vec![sock("10.1.1.1:80")]);
}
#[test]
fn missing_endpoints_or_port_resolve_empty() {
    let st = web_state(json!(8080), json!({"name": "http", "port": 8080}));
    let mut no_ep = st.clone();
    no_ep.endpoints.clear();
    assert!(resolve_with_state(&no_ep, &UpstreamRef::port("default/web", 80)).is_empty());
    assert!(resolve_with_state(&st, &UpstreamRef::port("default/web", 81)).is_empty());
    assert!(resolve_with_state(&st, &UpstreamRef::port("default/ghost", 80)).is_empty());
}
#[test]
fn ns_name_and_bare_name_lookup() {
    let mut st = ResolverState::default();
    let spec = json!({"type": "NodePort", "ports": [{"port": 80, "nodePort": 30080}]});
    for ns in ["aaa", "bbb"] {
        let v = parse_service(&svc("one", ns, spec.clone())).unwrap();
        st.services.insert(format!("{ns}/one"), v);
    }
    assert_eq!(find_service(&st, "one").unwrap().0, "aaa/one"); // BTreeMap order
    assert_eq!(find_service(&st, "bbb/one").unwrap().0, "bbb/one"); // exact wins
}
