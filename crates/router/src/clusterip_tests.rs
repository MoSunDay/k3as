//! Unit tests for the ClusterIP VIP plane ([`crate::clusterip`],
//! T4.3 Scope C / Q44): desired-listener planning only (alias/bind paths
//! need a live `lo` + `ip(1)` and are covered by the Sprint 51 e2e).

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use crate::clusterip::desired_vip_listeners;
use crate::endpoints::{ResolverState, ServiceView, SvcPort};
use crate::route::PortRef;

fn svc(kind: &str, vip: Option<Ipv4Addr>, ports: Vec<SvcPort>) -> ServiceView {
    ServiceView {
        namespace: "default".into(),
        name: "web".into(),
        kind_type: kind.into(),
        ports,
        ingress_ip: None,
        cluster_ip: vip,
    }
}

fn svc_port(port: u16) -> SvcPort {
    SvcPort {
        name: None,
        protocol: None,
        port,
        target_port: None,
        node_port: None,
    }
}

fn svc_port_proto(port: u16, proto: &str) -> SvcPort {
    SvcPort {
        protocol: Some(proto.into()),
        ..svc_port(port)
    }
}

fn vip() -> Ipv4Addr {
    "10.43.0.7".parse().unwrap()
}

#[test]
fn desired_per_allocated_vip_and_port() {
    let mut st = ResolverState::default();
    st.services.insert(
        "default/web".into(),
        svc("ClusterIP", Some(vip()), vec![svc_port(80)]),
    );
    let d = desired_vip_listeners(&st);
    assert_eq!(d.len(), 1);
    let up = &d[&SocketAddr::new(IpAddr::V4(vip()), 80)];
    assert_eq!(up.service, "default/web");
    assert_eq!(up.port, PortRef::Number(80));
}

#[test]
fn desired_skips_headless() {
    let mut st = ResolverState::default();
    st.services.insert(
        "default/headless".into(),
        svc("ClusterIP", None, vec![svc_port(80)]),
    );
    assert!(desired_vip_listeners(&st).is_empty());
}

#[test]
fn desired_skips_externalname_even_with_ip() {
    let mut st = ResolverState::default();
    st.services.insert(
        "default/en".into(),
        svc("ExternalName", Some(vip()), vec![svc_port(80)]),
    );
    assert!(desired_vip_listeners(&st).is_empty());
}

#[test]
fn desired_skips_udp_ports() {
    let mut st = ResolverState::default();
    st.services.insert(
        "default/dns".into(),
        svc(
            "ClusterIP",
            Some(vip()),
            vec![svc_port_proto(53, "UDP"), svc_port_proto(53, "TCP")],
        ),
    );
    let d = desired_vip_listeners(&st);
    assert_eq!(d.len(), 1, "only the TCP entry: {d:?}");
    assert!(d.contains_key(&SocketAddr::new(IpAddr::V4(vip()), 53)));
}

#[test]
fn desired_multiple_ports_same_vip() {
    let mut st = ResolverState::default();
    st.services.insert(
        "default/multi".into(),
        svc("ClusterIP", Some(vip()), vec![svc_port(80), svc_port(443)]),
    );
    let d = desired_vip_listeners(&st);
    assert_eq!(d.len(), 2, "two entries, same IP: {d:?}");
    assert!(d.contains_key(&SocketAddr::new(IpAddr::V4(vip()), 80)));
    assert!(d.contains_key(&SocketAddr::new(IpAddr::V4(vip()), 443)));
}

#[test]
fn desired_nodeport_service_with_clusterip_contributes() {
    // Upstream parity (Q44): NodePort-type Services ride the VIP plane too.
    let mut st = ResolverState::default();
    let mut np = svc_port(80);
    np.node_port = Some(30080);
    st.services
        .insert("default/np".into(), svc("NodePort", Some(vip()), vec![np]));
    let d = desired_vip_listeners(&st);
    assert_eq!(d.len(), 1);
    assert!(d.contains_key(&SocketAddr::new(IpAddr::V4(vip()), 80)));
}

/// T5.4 hardening: the bind retry tick is armed only while some desired
/// VIP listener is still unbound (a fully bound plane stays quiet).
#[test]
fn retry_tick_only_while_listeners_are_missing() {
    use crate::clusterip::retry_pending;
    assert!(retry_pending(2, 0), "nothing bound yet");
    assert!(retry_pending(2, 1), "one still missing");
    assert!(!retry_pending(2, 2), "fully bound: stay quiet");
    assert!(!retry_pending(0, 0), "empty plane: stay quiet");
}
