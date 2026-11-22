//! Pure compile cases for [`crate::netpol_compile`] (TODO **T4.4**,
//! decision **Q64**) — split out of the module for the file-size cap.
//! Fixtures mirror the apiserver list bodies the keeper fetches.

use serde_json::{json, Value};

use crate::netpol_compile::{compile_netpol, NetpolProgram, DISPATCH_CHAIN};

const WEB_UID: &str = "11111111-2222-3333-4444-555555555555";
const API_UID: &str = "aaaaaaaabbbbccccddddeeeeeeeeeeee";
// chain suffixes (first 12 hex of the UID)
const WEB_CHAIN_I: &str = "INIT-PRO-NPOL-I111111112222";

fn pod(name: &str, uid: &str, ip: &str, node: &str, labels: Value) -> Value {
    json!({
        "metadata": {"name": name, "namespace": "default", "uid": uid, "labels": labels},
        "spec": {"nodeName": node},
        "status": {"podIP": ip},
    })
}

fn pods(items: Vec<Value>) -> Value {
    json!({"items": items})
}

fn nss(items: Value) -> Value {
    items
}

/// The keeper passes the raw apiserver list body; these fixtures already
/// hand the ITEMS array in, so wrap it (`{"items": [...]}`).
fn policies(items: Value) -> Value {
    json!({ "items": items })
}

fn rules_of(prog: &NetpolProgram, chain: &str) -> Vec<String> {
    prog.chains.get(chain).cloned().unwrap_or_default()
}

/// NP1 shape: an empty-podSelector Ingress policy isolates every pod in
/// the namespace; no rules => conntrack + terminal REJECT (default deny).
#[test]
fn default_deny_ingress() {
    let web = pod("web", WEB_UID, "10.42.0.5", "n1", json!({"app": "web"}));
    let prog = compile_netpol(
        &pods(vec![web]),
        &policies(
            json!([{"metadata": {"name": "deny-all", "namespace": "default"},
            "spec": {"podSelector": {}, "policyTypes": ["Ingress"]}}]),
        ),
        &nss(json!({"items": [{"metadata": {"name": "default"}}]})),
        "n1",
    );
    assert_eq!(
        rules_of(&prog, WEB_CHAIN_I),
        vec![
            "-m conntrack --ctstate RELATED,ESTABLISHED -j ACCEPT".to_string(),
            "-j REJECT".to_string(),
        ]
    );
    // dispatch: destination match on the pod IP, then the terminal RETURN
    assert_eq!(
        rules_of(&prog, DISPATCH_CHAIN),
        vec![
            format!("-d 10.42.0.5/32 -j {WEB_CHAIN_I}"),
            "-j RETURN".to_string(),
        ]
    );
    // not egress-isolated: no E chain for this pod
    assert!(!prog.chains.contains_key("INIT-PRO-NPOL-E111111112222"));
}

/// NP2 shape: selected pods allow labeled peers on a port; non-selected
/// pods stay untouched (default allow, upstream).
#[test]
fn selected_allow_with_port() {
    let web = pod("web", WEB_UID, "10.42.0.5", "n1", json!({"app": "web"}));
    let other = pod("other", API_UID, "10.42.0.9", "n1", json!({"app": "other"}));
    let api = pod(
        "api",
        "99999999-8888-7777-6666-555555555555",
        "10.42.1.3",
        "n2",
        json!({"app": "api"}),
    );
    let prog = compile_netpol(
        &pods(vec![web.clone(), other, api]),
        &policies(
            json!([{"metadata": {"name": "web-ing", "namespace": "default"},
            "spec": {"podSelector": {"matchLabels": {"app": "web"}},
                     "ingress": [{"from": [{"podSelector": {"matchLabels": {"app": "api"}}}],
                                  "ports": [{"protocol": "TCP", "port": 8080}]}]}}]),
        ),
        &nss(json!({"items": [{"metadata": {"name": "default"}}]})),
        "n1",
    );
    assert_eq!(
        rules_of(&prog, WEB_CHAIN_I),
        vec![
            "-m conntrack --ctstate RELATED,ESTABLISHED -j ACCEPT".to_string(),
            "-s 10.42.1.3/32 -p tcp --dport 8080 -j ACCEPT".to_string(),
            "-j REJECT".to_string(),
        ]
    );
    // `other` is on n1 but not selected: only the dispatch RETURN exists.
    assert_eq!(prog.chains.len(), 2); // dispatch + web I chain
    let _ = web;
}

/// NP3 shape: egress to an ipBlock with `except`; policyTypes defaulting
/// (no policyTypes => Ingress only... but egress rules ADD Egress).
#[test]
fn egress_ipblock_and_defaulting() {
    let web = pod("web", WEB_UID, "10.42.0.5", "n1", json!({"app": "web"}));
    let prog = compile_netpol(
        &pods(vec![web]),
        &policies(json!([{"metadata": {"name": "eg", "namespace": "default"},
            "spec": {"podSelector": {},
                     "egress": [{"to": [{"ipBlock": {"cidr": "10.0.0.0/8",
                                                     "except": ["10.1.0.0/16"]}}]}]}}])),
        &nss(json!({"items": [{"metadata": {"name": "default"}}]})),
        "n1",
    );
    // defaulting: egress rules present => BOTH directions policed
    let egress_chain = "INIT-PRO-NPOL-E111111112222";
    let egress = rules_of(&prog, egress_chain);
    // 10.0.0.0/8 minus 10.1.0.0/16 => canonical CIDR split around the hole
    let accepts: Vec<&String> = egress.iter().filter(|l| l.starts_with("-d ")).collect();
    assert_eq!(accepts.len(), 8);
    assert_eq!(
        accepts.iter().map(|l| l.as_str()).collect::<Vec<_>>(),
        vec![
            "-d 10.0.0.0/16 -j ACCEPT",
            "-d 10.128.0.0/9 -j ACCEPT",
            "-d 10.16.0.0/12 -j ACCEPT",
            "-d 10.2.0.0/15 -j ACCEPT",
            "-d 10.32.0.0/11 -j ACCEPT",
            "-d 10.4.0.0/14 -j ACCEPT",
            "-d 10.64.0.0/10 -j ACCEPT",
            "-d 10.8.0.0/13 -j ACCEPT",
        ]
    );
    assert_eq!(egress.last().unwrap(), "-j REJECT");
    // ingress default-isolated too (policyTypes defaulted to both)
    assert!(prog.chains.contains_key(WEB_CHAIN_I));
    // dispatch gained the source match for egress
    let dispatch = rules_of(&prog, DISPATCH_CHAIN);
    assert!(dispatch.contains(&format!("-s 10.42.0.5/32 -j {egress_chain}")));
}

/// A rule with NO peers allows all addresses (upstream); ports-less
/// rules allow every port/protocol.
#[test]
fn peerless_rule_allows_all() {
    let web = pod("web", WEB_UID, "10.42.0.5", "n1", json!({}));
    let prog = compile_netpol(
        &pods(vec![web]),
        &policies(
            json!([{"metadata": {"name": "open", "namespace": "default"},
            "spec": {"podSelector": {}, "ingress": [{"ports": [
                {"protocol": "UDP", "port": 53}]}]}}]),
        ),
        &nss(json!({"items": [{"metadata": {"name": "default"}}]})),
        "n1",
    );
    assert_eq!(
        rules_of(&prog, WEB_CHAIN_I),
        vec![
            "-m conntrack --ctstate RELATED,ESTABLISHED -j ACCEPT".to_string(),
            "-p udp --dport 53 -j ACCEPT".to_string(),
            "-j REJECT".to_string(),
        ]
    );
}

/// Q52 exclusions: hostNetwork pods and pods without a podIP are never
/// policed; pods on other nodes are not this node's problem.
#[test]
fn exclusions_and_foreign_nodes() {
    let hostnet = {
        let mut p = pod(
            "h",
            "ffffffffeeeeddddccccbbbbaaaa0000",
            "192.168.1.10",
            "n1",
            json!({}),
        );
        p["spec"]["hostNetwork"] = json!(true);
        p
    };
    let pending = {
        let mut p = pod("p", "0123456789abcdef0123456789abcdef", "", "n1", json!({}));
        p.as_object_mut().unwrap().remove("status");
        p
    };
    let foreign = pod(
        "f",
        "12345678123456781234567812345678",
        "10.42.9.9",
        "n2",
        json!({}),
    );
    let prog = compile_netpol(
        &pods(vec![hostnet, pending, foreign]),
        &policies(json!([{"metadata": {"name": "all", "namespace": "default"},
            "spec": {"podSelector": {}}}])),
        &nss(json!({"items": [{"metadata": {"name": "default"}}]})),
        "n1",
    );
    // nothing policed on n1: the dispatch chain holds only its RETURN
    assert_eq!(
        rules_of(&prog, DISPATCH_CHAIN),
        vec!["-j RETURN".to_string()]
    );
    assert_eq!(prog.chains.len(), 1);
}

/// namespaceSelector peers reach across namespaces (empty selector =
/// every namespace); the match uses namespace LABELS, not names.
#[test]
fn namespace_selector_peer() {
    let web = pod("web", WEB_UID, "10.42.0.5", "n1", json!({"app": "web"}));
    let tenant = {
        let mut p = pod(
            "t",
            "abababababababababababababababab",
            "10.42.2.7",
            "n2",
            json!({"role": "db"}),
        );
        p["metadata"]["namespace"] = json!("tenant-a");
        p
    };
    let prog = compile_netpol(
        &pods(vec![web, tenant]),
        &policies(json!([{"metadata": {"name": "x", "namespace": "default"},
            "spec": {"podSelector": {},
                     "ingress": [{"from": [{"namespaceSelector": {"matchLabels": {"tier": "gold"}}}]}]}}])),
        &nss(json!({"items": [
            {"metadata": {"name": "default"}},
            {"metadata": {"name": "tenant-a", "labels": {"tier": "gold"}}},
        ]})),
        "n1",
    );
    assert_eq!(
        rules_of(&prog, WEB_CHAIN_I),
        vec![
            "-m conntrack --ctstate RELATED,ESTABLISHED -j ACCEPT".to_string(),
            "-s 10.42.2.7/32 -j ACCEPT".to_string(),
            "-j REJECT".to_string(),
        ]
    );
}
