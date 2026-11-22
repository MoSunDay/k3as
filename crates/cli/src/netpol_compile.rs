//! NetworkPolicy enforcement compiler (TODO **T4.4**, decision **Q64**):
//! pure JSON -> iptables rule program.
//!
//! Input: the cluster-wide `pods` / `namespaces` lists plus the
//! `networkpolicies` list (apiserver cluster-path collections, Q10
//! JSON-only) and this node's name. Output: the [`NetpolProgram`] — a
//! deterministic `chain -> rule lines` map that
//! [`crate::netpol_iptables`] renders into one `iptables-restore`
//! payload (dispatch chain `INIT-PRO-NPOL-FWD` + per-pod
//! `INIT-PRO-NPOL-{I,E}<12hex>` chains, 27 chars <= iptables' 28).
//!
//! Semantics (upstream networking.k8s.io/v1, supported subset Q59/Q64):
//! - a pod is ISOLATED for a direction when >= 1 policy selecting it
//!   polices that direction (`policyTypes` after upstream defaulting);
//!   the allow set is the union over ALL selecting policies of
//!   (peer x port);
//! - a rule with NO peers allows all addresses for its ports (upstream);
//! - peers expand to concrete IPv4 CIDRs: podSelector/namespaceSelector
//!   -> podIPs of matching pods cluster-wide, ipBlock -> `cidr` minus
//!   `except` ([`crate::netpol_cidr`]);
//! - deny is `-j REJECT` (fail-fast: probes see refusal, not a silent
//!   drop), preceded by a conntrack `RELATED,ESTABLISHED` ACCEPT so
//!   replies to allowed flows survive (upstream posture).
//!
//! Exclusions (Q52): `hostNetwork` pods and pods without a podIP are
//! never policed (no stable address); builtin-engine nodes never spawn
//! the keeper at all — those pods share the host netns (see the spawn
//! gate in `runtime::run_supervised`).
//!
//! Pure module: no I/O, no clocks — output is a function of the JSON
//! inputs (testable without iptables).

use std::collections::{BTreeMap, BTreeSet};

use api::{effective_policy_types, label_selector_matches};
use serde_json::Value;

use crate::netpol_cidr::subtract_cidr;

/// iptables chain prefix for every T4.4 chain (also the teardown/prune
/// marker in [`crate::netpol_iptables`]).
pub(crate) const CHAIN_PREFIX: &str = "INIT-PRO-NPOL-";
/// Dispatch chain jumped from `FORWARD` (inserted via `iptables -I`,
/// NOT via restore — see [`crate::netpol_iptables::ensure_dispatch_jump`]).
pub(crate) const DISPATCH_CHAIN: &str = "INIT-PRO-NPOL-FWD";

/// The compiled rule program: chain name -> the `-A <chain> ...` rule
/// bodies (the dispatch chain included). `BTreeMap` keeps the rendered
/// payload deterministic (idempotent re-applies, stable tests).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct NetpolProgram {
    pub chains: BTreeMap<String, Vec<String>>,
}

/// The pod facts the compiler needs (one row per policed pod candidate).
#[derive(Debug, Clone)]
struct PodFacts {
    ns: String,
    uid: String,
    ip: String,
    node: String,
    labels: Value,
}

/// 12-hex chain suffix from a pod UID (UUIDs strip to 32 hex chars; the
/// first 12 identify the per-pod chains — 27-char names stay under the
/// 28-char iptables limit).
pub(crate) fn chain_suffix(uid: &str) -> String {
    let mut hex: String = uid
        .chars()
        .filter(|c| c.is_ascii_hexdigit())
        .take(12)
        .flat_map(char::to_lowercase)
        .collect();
    while hex.len() < 12 {
        hex.push('0');
    }
    hex
}

/// Parse a `PodList` into policed pod candidates: every pod with an IPv4
/// `status.podIP` that is NOT `hostNetwork` (Q52 exclusion). `nodeName`
/// is carried so [`compile_netpol`] can filter to this node.
fn collect_pods(pods_list: &Value) -> Vec<PodFacts> {
    pods_list
        .pointer("/items")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|pod| pod.pointer("/spec/hostNetwork").and_then(Value::as_bool) != Some(true))
        .filter(|pod| {
            pod.pointer("/status/podIP")
                .and_then(Value::as_str)
                .is_some_and(|ip| ip.parse::<std::net::Ipv4Addr>().is_ok())
        })
        .map(|pod| {
            let meta = pod.pointer("/metadata").unwrap_or(&Value::Null);
            let field = |k: &str| {
                meta.get(k)
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string()
            };
            PodFacts {
                ns: field("namespace"),
                uid: field("uid"),
                ip: pod
                    .pointer("/status/podIP")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                node: pod
                    .pointer("/spec/nodeName")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                labels: meta.get("labels").cloned().unwrap_or(Value::Null),
            }
        })
        .collect()
}

/// `namespace -> labels` from a `NamespaceList` (namespaceSelector
/// peer matching).
fn collect_ns_labels(ns_list: &Value) -> BTreeMap<String, Value> {
    ns_list
        .pointer("/items")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|ns| {
            ns.pointer("/metadata/name")
                .and_then(Value::as_str)
                .map(|name| {
                    (
                        name.to_string(),
                        ns.pointer("/metadata/labels")
                            .cloned()
                            .unwrap_or(Value::Null),
                    )
                })
        })
        .collect()
}

/// `(proto, port)` allow entry; `None` = all ports AND protocols (a rule
/// without `ports`, upstream). Named ports / SCTP were rejected at
/// admission (Q64); anything non-numeric that still reaches the compiler
/// is SKIPPED — fail-closed, never fail-open.
type PortEntry = Option<(&'static str, u16)>;

fn port_entries(ports: Option<&Vec<Value>>) -> Vec<PortEntry> {
    let Some(entries) = ports.filter(|p| !p.is_empty()) else {
        return vec![None];
    };
    entries
        .iter()
        .filter_map(|p| -> Option<PortEntry> {
            let proto = match p.get("protocol").and_then(Value::as_str) {
                Some("UDP") => "udp",
                _ => "tcp", // upstream default protocol is TCP
            };
            let port = p
                .get("port")
                .and_then(Value::as_i64)
                .filter(|n| (1..=65535).contains(n))?;
            Some(Some((proto, port as u16)))
        })
        .collect()
}

/// Expand ONE peer to concrete IPv4 CIDRs (Q64 subset: selectors ->
/// matching pod IPs, ipBlock -> cidr minus except; peer-shape
/// exclusivity was validated at admission).
fn peer_cidrs(
    peer: &Value,
    policy_ns: &str,
    pods: &[PodFacts],
    ns_labels: &BTreeMap<String, Value>,
) -> Vec<String> {
    if let Some(block) = peer.get("ipBlock") {
        let Some(cidr) = block.get("cidr").and_then(Value::as_str) else {
            return Vec::new();
        };
        let except: Vec<String> = block
            .get("except")
            .and_then(Value::as_array)
            .map(|xs| {
                xs.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        return subtract_cidr(cidr, &except);
    }
    // Selector peer: namespace scope first (absent = the policy's own
    // namespace; an EMPTY namespaceSelector object = every namespace),
    // then the podSelector inside that scope (empty = every pod).
    let ns_sel = peer.get("namespaceSelector");
    let pod_sel = peer.get("podSelector").cloned().unwrap_or(Value::Null);
    pods.iter()
        .filter(|p| match ns_sel {
            Some(sel) => label_selector_matches(ns_labels.get(&p.ns).map(|v| v as &Value), sel),
            None => p.ns == policy_ns,
        })
        .filter(|p| label_selector_matches(Some(&p.labels), &pod_sel))
        .map(|p| format!("{}/32", p.ip))
        .collect()
}

/// Union of a rule's peers' CIDRs; a rule with NO (or an empty) peer
/// list allows every address (upstream).
fn rule_peer_cidrs(
    peers: Option<&Vec<Value>>,
    policy_ns: &str,
    pods: &[PodFacts],
    ns_labels: &BTreeMap<String, Value>,
) -> Vec<String> {
    let Some(peers) = peers.filter(|p| !p.is_empty()) else {
        return vec!["0.0.0.0/0".to_string()];
    };
    let mut set = BTreeSet::new();
    for peer in peers {
        set.extend(peer_cidrs(peer, policy_ns, pods, ns_labels));
    }
    set.into_iter().collect()
}

/// Allow-rule bodies for one (peer-cidr x port-entry) cross product.
/// `peer_field` selects the direction: ingress matches the peer as the
/// source (`-s`), egress as the destination (`-d`); the catch-all
/// `0.0.0.0/0` peer omits the address match entirely.
fn allow_lines(peer: &str, ports: &[PortEntry], peer_field: &str) -> Vec<String> {
    let peer_part = if peer != "0.0.0.0/0" {
        format!("-{peer_field} {peer} ")
    } else {
        String::new()
    };
    ports
        .iter()
        .map(|entry| match entry {
            Some((proto, port)) => format!("{peer_part}-p {proto} --dport {port} -j ACCEPT"),
            None => format!("{peer_part}-j ACCEPT"),
        })
        .collect()
}

/// Compile the full node program (see the module docs for semantics).
pub(crate) fn compile_netpol(
    pods_list: &Value,
    policies_list: &Value,
    namespaces_list: &Value,
    node: &str,
) -> NetpolProgram {
    let pods = collect_pods(pods_list);
    let ns_labels = collect_ns_labels(namespaces_list);
    let policies: Vec<Value> = policies_list
        .pointer("/items")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let mut chains: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut dispatch: Vec<String> = Vec::new();
    for pod in pods.iter().filter(|p| p.node == node) {
        // Policies selecting this pod: same namespace + podSelector match
        // (an EMPTY podSelector selects every pod in the namespace —
        // upstream NetworkPolicy semantics).
        let selecting: Vec<&Value> = policies
            .iter()
            .filter(|p| {
                p.pointer("/metadata/namespace").and_then(Value::as_str) == Some(pod.ns.as_str())
                    && label_selector_matches(
                        Some(&pod.labels),
                        p.pointer("/spec/podSelector").unwrap_or(&Value::Null),
                    )
            })
            .collect();
        // (direction_kind, peer_field, addr_field, rules_key, peers_key):
        // ingress dispatches on the pod as destination and peers are
        // sources; egress is the mirror image.
        for (kind, peer_field, addr_field, rules_key, peers_key) in [
            ("I", "s", "d", "ingress", "from"),
            ("E", "d", "s", "egress", "to"),
        ] {
            let policed: Vec<&&Value> = selecting
                .iter()
                .filter(|p| {
                    let (ingress, egress) =
                        effective_policy_types(p.pointer("/spec").unwrap_or(&Value::Null));
                    if kind == "I" {
                        ingress
                    } else {
                        egress
                    }
                })
                .collect();
            if policed.is_empty() {
                continue; // not isolated for this direction: default allow
            }
            let chain = format!("{CHAIN_PREFIX}{kind}{}", chain_suffix(&pod.uid));
            let mut lines: BTreeSet<String> = BTreeSet::new();
            for policy in policed {
                let policy_ns = policy
                    .pointer("/metadata/namespace")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                for rule in policy
                    .pointer(&format!("/spec/{rules_key}"))
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    let peers = rule
                        .get(peers_key)
                        .and_then(Value::as_array)
                        .cloned()
                        .map(|xs| xs.into_iter().collect::<Vec<_>>());
                    let ports = rule
                        .get("ports")
                        .and_then(Value::as_array)
                        .cloned()
                        .map(|xs| xs.into_iter().collect::<Vec<_>>());
                    for peer in rule_peer_cidrs(peers.as_ref(), policy_ns, &pods, &ns_labels) {
                        lines.extend(allow_lines(
                            &peer,
                            &port_entries(ports.as_ref()),
                            peer_field,
                        ));
                    }
                }
            }
            let mut body = vec!["-m conntrack --ctstate RELATED,ESTABLISHED -j ACCEPT".to_string()];
            body.extend(lines);
            body.push("-j REJECT".to_string());
            chains.insert(chain.clone(), body);
            dispatch.push(format!("-{addr_field} {}/32 -j {chain}", pod.ip));
        }
    }
    dispatch.push("-j RETURN".to_string());
    chains.insert(DISPATCH_CHAIN.to_string(), dispatch);
    NetpolProgram { chains }
}
