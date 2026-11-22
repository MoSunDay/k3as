//! NetworkPolicy semantics + admission validation (TODO **T4.4**, decision
//! **Q64**, supported-subset scope **Q59**).
//!
//! Three pure functions shared by the apiserver (admission) and the
//! node-side enforcement keeper (compile):
//! - [`label_selector_matches`] — upstream `meta/v1.LabelSelector`
//!   matching where an EMPTY selector matches EVERYTHING (the NetworkPolicy
//!   rule; the controllers' select-nothing posture is a wrapper over this,
//!   see `controllers::object::selector_matches`).
//! - [`effective_policy_types`] — upstream `policyTypes` defaulting:
//!   unset => `[Ingress]`, plus `Egress` when any egress rule is present.
//! - [`validate_network_policy`] — the supported-subset admission gate:
//!   protocol TCP/UDP only (SCTP rejected), numeric ports only (named
//!   ports rejected), `ipBlock` IPv4 with `except` contained in `cidr`,
//!   peers non-empty, `ipBlock` exclusive of the two selectors,
//!   `policyTypes` values in `{Ingress, Egress}`.

use serde_json::Value;

/// Upstream `meta/v1.LabelSelector` match: `matchLabels` (equality) AND
/// `matchExpressions` (`In`/`NotIn`/`Exists`/`DoesNotExist`), where an
/// empty or absent selector matches EVERYTHING (NetworkPolicy semantics).
/// Anything that is not an object is treated as the empty selector.
pub fn label_selector_matches(labels: Option<&Value>, selector: &Value) -> bool {
    let sel = selector.as_object();
    let labels_obj = labels.and_then(Value::as_object);
    if let Some(ml) = sel
        .and_then(|s| s.get("matchLabels"))
        .and_then(Value::as_object)
    {
        for (k, want) in ml {
            if labels_obj.and_then(|l| l.get(k)).and_then(Value::as_str) != want.as_str() {
                return false;
            }
        }
    }
    if let Some(exprs) = sel
        .and_then(|s| s.get("matchExpressions"))
        .and_then(Value::as_array)
    {
        for expr in exprs {
            if !expression_matches(labels, expr) {
                return false;
            }
        }
    }
    true
}

/// One `matchExpressions` entry (identical operator set to the controllers'
/// matcher; shared semantics, no duplication — Q64 reuse note).
fn expression_matches(labels: Option<&Value>, expr: &Value) -> bool {
    let (Some(key), Some(op)) = (
        expr.get("key").and_then(Value::as_str),
        expr.get("operator").and_then(Value::as_str),
    ) else {
        return false;
    };
    let value = labels.and_then(|l| l.get(key)).and_then(Value::as_str);
    let listed = |v: &str| {
        expr.get("values")
            .and_then(Value::as_array)
            .map(|vs| vs.iter().any(|x| x.as_str() == Some(v)))
    };
    match op {
        "Exists" => value.is_some(),
        "DoesNotExist" => value.is_none(),
        "In" => value.map(|v| listed(v).unwrap_or(false)).unwrap_or(false),
        "NotIn" => value.map(|v| !listed(v).unwrap_or(false)).unwrap_or(true),
        _ => false, // unknown operator: no match
    }
}

/// Which directions a policy actually polices after upstream defaulting:
/// `(ingress, egress)`. Unset/empty `policyTypes` => `[Ingress]`, plus
/// `Egress` when ANY egress rule is present.
pub fn effective_policy_types(spec: &Value) -> (bool, bool) {
    let explicit = spec
        .get("policyTypes")
        .and_then(Value::as_array)
        .map(|ts| {
            ts.iter()
                .filter_map(|t| t.as_str().to_owned())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if explicit.is_empty() {
        let egress_rules = spec
            .get("egress")
            .and_then(Value::as_array)
            .is_some_and(|e| !e.is_empty());
        return (true, egress_rules);
    }
    (explicit.contains(&"Ingress"), explicit.contains(&"Egress"))
}

/// Supported-subset admission for one NetworkPolicy body (Q64). Returns the
/// upstream-shaped rejection message on `Err`. Scope locks: `protocol`
/// TCP/UDP (SCTP out), numeric `port` only (named ports out), `ipBlock`
/// IPv4-only with `except` ⊆ `cidr`.
pub fn validate_network_policy(body: &Value) -> Result<(), String> {
    let Some(spec) = body.get("spec") else {
        return Ok(()); // nothing to check without a spec
    };
    for (key, rules_key, peers_key) in [("ingress", "ingress", "from"), ("egress", "egress", "to")]
    {
        let Some(rules) = spec.get(rules_key).and_then(Value::as_array) else {
            continue;
        };
        for rule in rules {
            if let Some(ports) = rule.get("ports").and_then(Value::as_array) {
                for p in ports {
                    validate_port(p)?;
                }
            }
            if let Some(peers) = rule.get(peers_key).and_then(Value::as_array) {
                for peer in peers {
                    validate_peer(peer, key)?;
                }
            }
        }
    }
    if let Some(types) = spec.get("policyTypes").and_then(Value::as_array) {
        for t in types {
            let Some(t) = t.as_str() else {
                return Err(format!("spec.policyTypes[] must be a string, got {t}"));
            };
            if t != "Ingress" && t != "Egress" {
                return Err(format!(
                    "unsupported spec.policyTypes[] value {t:?}: supported values are \"Ingress\", \"Egress\" (Q64)"
                ));
            }
        }
    }
    Ok(())
}

/// One `ports[]` entry: protocol TCP/UDP (SCTP out of subset), numeric
/// port 1-65535 (named ports out of subset).
fn validate_port(p: &Value) -> Result<(), String> {
    if let Some(proto) = p.get("protocol").and_then(Value::as_str) {
        match proto {
            "TCP" | "UDP" => {}
            "SCTP" => {
                return Err(
                    "protocol SCTP is out of the supported subset (spec.ports[].protocol: TCP|UDP only, Q64)"
                        .to_string(),
                );
            }
            other => {
                return Err(format!(
                    "unsupported spec.ports[].protocol {other:?}: supported values are \"TCP\", \"UDP\" (Q64)"
                ));
            }
        }
    }
    match p.get("port") {
        None | Some(Value::Null) => Ok(()), // all ports on the (default TCP) protocol
        Some(Value::Number(n)) => {
            let ok = n.as_i64().is_some_and(|v| (1..=65535).contains(&v));
            if ok {
                Ok(())
            } else {
                Err(format!(
                    "spec.ports[].port must be an integer in [1, 65535], got {n} (Q64)"
                ))
            }
        }
        Some(other) => Err(format!(
            "named ports are out of the supported subset (spec.ports[].port must be numeric, got {other}, Q64)"
        )),
    }
}

/// One `from[]`/`to[]` peer: at least one of podSelector/namespaceSelector/
/// ipBlock; ipBlock exclusive of both; ipBlock.cidr parses as IPv4 CIDR and
/// every `except` entry is a contained IPv4 CIDR.
fn validate_peer(peer: &Value, direction: &str) -> Result<(), String> {
    let path = format!("spec.{direction}[].{{from|to}}[]");
    let has_pod = peer.get("podSelector").is_some_and(|v| v != &Value::Null);
    let has_ns = peer
        .get("namespaceSelector")
        .is_some_and(|v| v != &Value::Null);
    let has_ip = peer.get("ipBlock").is_some_and(|v| v != &Value::Null);
    if !has_pod && !has_ns && !has_ip {
        return Err(format!(
            "{path} peer must set at least one of podSelector, namespaceSelector, ipBlock"
        ));
    }
    if let Some(ip) = peer.get("ipBlock") {
        if has_pod || has_ns {
            return Err(format!(
                "{path} peer ipBlock cannot be combined with podSelector/namespaceSelector"
            ));
        }
        let Some(cidr) = ip.get("cidr").and_then(Value::as_str) else {
            return Err(format!("{path} peer ipBlock.cidr is required"));
        };
        let Some(base) = ipv4_cidr_range(cidr) else {
            return Err(format!(
                "{path} peer ipBlock.cidr must be an IPv4 CIDR (e.g. 10.42.0.0/24), got {cidr:?} (Q64)"
            ));
        };
        if let Some(excepts) = ip.get("except").and_then(Value::as_array) {
            for e in excepts {
                let Some(es) = e.as_str() else {
                    return Err(format!("{path} peer ipBlock.except[] must be CIDR strings"));
                };
                match ipv4_cidr_range(es) {
                    Some(hole) if hole.0 >= base.0 && hole.1 <= base.1 => {}
                    _ => {
                        return Err(format!(
                            "{path} peer ipBlock.except[] {es:?} is not an IPv4 CIDR contained in {cidr}"
                        ));
                    }
                }
            }
        }
    }
    Ok(())
}

/// `(first, last)` inclusive address range of an IPv4 CIDR (None = not
/// parseable). Shared with the enforcement compiler through the wire JSON.
pub fn ipv4_cidr_range(cidr: &str) -> Option<(u32, u32)> {
    let (addr, prefix) = cidr.split_once('/')?;
    let prefix: u32 = prefix.parse().ok()?;
    if prefix > 32 {
        return None;
    }
    let octets: Vec<u32> = addr
        .split('.')
        .map(|o| o.parse::<u32>().ok().filter(|v| *v <= 255))
        .collect::<Option<Vec<_>>>()?;
    if octets.len() != 4 {
        return None;
    }
    let ip: u32 = (octets[0] << 24) | (octets[1] << 16) | (octets[2] << 8) | octets[3];
    let mask: u32 = if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - prefix)
    };
    Some((ip & mask, ip | !mask))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn empty_selector_matches_all() {
        let pod = json!({"metadata": {"labels": {"app": "web"}}});
        let labels = pod.pointer("/metadata/labels");
        assert!(label_selector_matches(labels, &json!({})));
        assert!(label_selector_matches(labels, &Value::Null));
        assert!(label_selector_matches(
            labels,
            &json!({"matchLabels": {"app": "web"}})
        ));
        assert!(!label_selector_matches(
            labels,
            &json!({"matchLabels": {"app": "api"}})
        ));
        assert!(label_selector_matches(
            None,
            &json!({"matchExpressions": [
                {"key": "app", "operator": "DoesNotExist"}
            ]})
        ));
    }

    #[test]
    fn effective_types_defaulting() {
        // unset -> Ingress; egress rules present -> +Egress.
        assert_eq!(effective_policy_types(&json!({})), (true, false));
        assert_eq!(
            effective_policy_types(&json!({"ingress": [{}]})),
            (true, false)
        );
        assert_eq!(
            effective_policy_types(&json!({"egress": [{}]})),
            (true, true)
        );
        // explicit wins, including neither.
        assert_eq!(
            effective_policy_types(&json!({"policyTypes": ["Egress"]})),
            (false, true)
        );
        assert_eq!(
            effective_policy_types(&json!({"policyTypes": ["Ingress", "Egress"]})),
            (true, true)
        );
    }

    #[test]
    fn admission_rejects_out_of_subset_shapes() {
        let np = |spec| json!({"apiVersion": "networking.k8s.io/v1", "kind": "NetworkPolicy", "metadata": {"name": "p"}, "spec": spec});
        // SCTP out.
        let err = validate_network_policy(&np(json!({
            "podSelector": {}, "ingress": [{"ports": [{"protocol": "SCTP", "port": 80}]}]
        })))
        .unwrap_err();
        assert!(err.contains("SCTP"), "{err}");
        // Named ports out.
        let err = validate_network_policy(&np(json!({
            "podSelector": {}, "ingress": [{"ports": [{"port": "http-tcp"}]}]
        })))
        .unwrap_err();
        assert!(err.contains("named ports"), "{err}");
        // Empty peer.
        let err = validate_network_policy(&np(json!({
            "podSelector": {}, "egress": [{"to": [{}]}]
        })))
        .unwrap_err();
        assert!(err.contains("at least one"), "{err}");
        // ipBlock + selectors.
        let err = validate_network_policy(&np(json!({
            "podSelector": {},
            "ingress": [{"from": [{"podSelector": {"matchLabels": {"a": "b"}}, "ipBlock": {"cidr": "10.0.0.0/8"}}]}]
        })))
        .unwrap_err();
        assert!(err.contains("cannot be combined"), "{err}");
        // except not contained.
        let err = validate_network_policy(&np(json!({
            "podSelector": {},
            "ingress": [{"from": [{"ipBlock": {"cidr": "10.42.0.0/24", "except": ["10.99.0.0/24"]}}]}]
        })))
        .unwrap_err();
        assert!(err.contains("contained"), "{err}");
        // Unknown policyTypes value.
        let err = validate_network_policy(&np(json!({
            "podSelector": {}, "policyTypes": ["Ingress", "Both"]
        })))
        .unwrap_err();
        assert!(err.contains("policyTypes"), "{err}");
        // Valid default-deny + ipBlock pass.
        assert!(validate_network_policy(&np(
            json!({"podSelector": {"matchLabels": {"app": "v"}}, "ingress": []})
        ))
        .is_ok());
        assert!(validate_network_policy(&np(json!({
            "podSelector": {},
            "ingress": [{"from": [{"ipBlock": {"cidr": "10.42.0.0/16", "except": ["10.42.1.0/24"]}}], "ports": [{"protocol": "UDP", "port": 53}]}]
        })))
        .is_ok());
    }

    #[test]
    fn cidr_ranges() {
        assert_eq!(
            ipv4_cidr_range("10.42.0.0/24"),
            Some((0x0a2a0000, 0x0a2a00ff))
        );
        assert_eq!(ipv4_cidr_range("0.0.0.0/0"), Some((0x00000000, 0xffffffff)));
        assert_eq!(
            ipv4_cidr_range("10.0.0.1/32"),
            Some((0x0a000001, 0x0a000001))
        );
        assert_eq!(ipv4_cidr_range("10.0.0.0/33"), None);
        assert_eq!(ipv4_cidr_range("10.0.0/8"), None);
        assert_eq!(ipv4_cidr_range("300.0.0.0/8"), None);
    }
}
