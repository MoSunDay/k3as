//! Service defaulting at create time (Sprint 18 / S3).
//!
//! On Service create `spec.type` defaults to `ClusterIP`; NodePort and
//! LoadBalancer services get missing `spec.ports[].nodePort`s allocated
//! lowest-free from the k8s range 30000-32767 by scanning existing
//! Services. Explicit nodePorts are validated (range, uniqueness per
//! protocol); a double allocation lost to a concurrent writer between LIST
//! and CREATE is healed post-create via CAS retry ([`heal_nodeport_collision`]).
//!
//! Decision **D**: NodePort-only dataplane in v1 — ClusterIP Services are
//! creatable/stored but non-forwarding; no `spec.clusterIP` is assigned
//! (JSON-only wire, **Q10**).

use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::sync::Arc;
use storage::{Key, KeyPrefix, StorageBackend, StorageError, StoredEntry};

/// Inclusive k8s NodePort range.
pub(crate) const NODE_PORT_MIN: u16 = 30000;
pub(crate) const NODE_PORT_MAX: u16 = 32767;

/// Bound on CAS retries when healing a post-create nodePort collision.
const HEAL_MAX_ATTEMPTS: u32 = 5;

/// Service defaulting failure (errors as values).
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ServiceError {
    /// 422-class: nodePort out of range, already allocated, or duplicated.
    Invalid(String),
}

/// NodePort-bearing types (k8s also gives LoadBalancers nodePorts).
fn is_nodeport_type(v: &Value) -> bool {
    let t = v.pointer("/spec/type").and_then(Value::as_str);
    matches!(t, Some("NodePort" | "LoadBalancer"))
}

/// Protocol of a port entry; k8s defaults to `"TCP"` when absent.
fn protocol_of(port: &Value) -> String {
    let p = port.get("protocol").and_then(Value::as_str);
    p.unwrap_or("TCP").to_string()
}

/// Read an explicit `nodePort` as a `u16`, if present and integral.
fn explicit_node_port(port: &Value) -> Option<Result<u16, ServiceError>> {
    let v = port.get("nodePort").filter(|v| !v.is_null())?;
    let n = v.as_u64().and_then(|n| u16::try_from(n).ok());
    let bad = ServiceError::Invalid("nodePort must be an integer".into());
    Some(n.ok_or(bad))
}

/// Entries of `spec.ports` (empty when absent).
fn spec_ports(v: &Value) -> &[Value] {
    let ports = v.pointer("/spec/ports").and_then(Value::as_array);
    ports.map(Vec::as_slice).unwrap_or_default()
}

/// Mutable [`spec_ports`].
fn spec_ports_mut(v: &mut Value) -> &mut [Value] {
    let ports = v.pointer_mut("/spec/ports");
    let arr = ports.and_then(Value::as_array_mut);
    arr.map(Vec::as_mut_slice).unwrap_or_default()
}

/// The `(protocol, nodePort)` pairs one Service holds.
fn node_ports_of(v: &Value) -> BTreeSet<(String, u16)> {
    let mut out = BTreeSet::new();
    if is_nodeport_type(v) {
        for p in spec_ports(v) {
            if let Some(Ok(n)) = explicit_node_port(p) {
                out.insert((protocol_of(p), n));
            }
        }
    }
    out
}

/// Every `(protocol, nodePort)` pair already taken across `services`.
pub(crate) fn used_node_ports(services: &[Value]) -> BTreeSet<(String, u16)> {
    services.iter().flat_map(node_ports_of).collect()
}

/// Lowest free nodePort for `proto` outside `used`.
fn alloc_free(proto: &str, used: &BTreeSet<(String, u16)>) -> Result<u16, ServiceError> {
    (NODE_PORT_MIN..=NODE_PORT_MAX)
        .find(|p| !used.contains(&(proto.to_string(), *p)))
        .ok_or_else(|| {
            let msg = format!("NodePort range {NODE_PORT_MIN}-{NODE_PORT_MAX} exhausted");
            ServiceError::Invalid(msg)
        })
}

/// Default `spec.type` and allocate/validate nodePorts IN PLACE: absent
/// type -> `ClusterIP`; `NodePort`/`LoadBalancer` ports get their explicit
/// `nodePort` validated (range + not taken + not duplicated per protocol)
/// or the lowest-free one allocated, marked in `used`; other types are
/// untouched beyond the type default.
pub(crate) fn default_service(
    body: &mut Value,
    used: &mut BTreeSet<(String, u16)>,
) -> Result<(), ServiceError> {
    let Some(root) = body.as_object_mut() else {
        return Ok(());
    };
    let spec = root.entry("spec").or_insert_with(|| json!({}));
    let Some(spec) = spec.as_object_mut() else {
        return Ok(()); // malformed spec: leave untouched
    };
    let t = spec.get("type").and_then(Value::as_str);
    let mut ty = t.unwrap_or("").to_string();
    if ty.is_empty() {
        ty = "ClusterIP".to_string();
        spec.insert("type".into(), json!("ClusterIP"));
    }
    if ty != "NodePort" && ty != "LoadBalancer" {
        return Ok(()); // ClusterIP/ExternalName/...: no nodePorts
    }
    let Some(ports) = spec.get_mut("ports").and_then(Value::as_array_mut) else {
        return Ok(()); // no ports to allocate
    };
    let mut ours: BTreeSet<(String, u16)> = BTreeSet::new();
    for port in ports.iter_mut() {
        let proto = protocol_of(port);
        // Resolve/validate the explicit nodePort before mutating (borrows).
        let explicit = match explicit_node_port(port) {
            None => None,
            Some(Err(e)) => return Err(e),
            Some(Ok(n)) => {
                if !(NODE_PORT_MIN..=NODE_PORT_MAX).contains(&n) {
                    return Err(ServiceError::Invalid(format!(
                        "nodePort {n} is out of range: must be between \
                         {NODE_PORT_MIN} and {NODE_PORT_MAX}"
                    )));
                }
                Some(n)
            }
        };
        let pair = match explicit {
            Some(n) => (proto.clone(), n),
            None => (proto.clone(), alloc_free(&proto, used)?),
        };
        if ours.contains(&pair) {
            return Err(ServiceError::Invalid(format!(
                "duplicate nodePort {} for protocol {}",
                pair.1, pair.0
            )));
        }
        if used.contains(&pair) {
            return Err(ServiceError::Invalid(format!(
                "nodePort {} for protocol {} is already allocated",
                pair.1, pair.0
            )));
        }
        ours.insert(pair.clone());
        used.insert(pair.clone());
        if explicit.is_none() {
            if let Some(obj) = port.as_object_mut() {
                obj.insert("nodePort".into(), json!(pair.1));
            }
        }
    }
    Ok(())
}

/// Create-time Service preparation: LIST services, then default in place.
pub(crate) async fn prepare(
    store: &Arc<dyn StorageBackend>,
    body: &mut Value,
) -> Result<(), ServiceError> {
    let mut used = BTreeSet::new();
    if let Ok(existing) = store.list(&KeyPrefix::new("", "services", None)).await {
        let vals: Vec<Value> = existing.into_iter().map(|e| e.value).collect();
        used = used_node_ports(&vals);
    }
    default_service(body, &mut used)
}

/// Post-create collision heal: if a DIFFERENT Service holds one of our
/// `(protocol, nodePort)` pairs (lost LIST/CREATE race), CAS-update us onto
/// fresh free ports (bounded retry). Non-NodePort services pass through;
/// never fails the request — worst case the object keeps its ports.
pub(crate) async fn heal_nodeport_collision(
    store: &Arc<dyn StorageBackend>,
    key: &Key,
    mut entry: StoredEntry,
) -> StoredEntry {
    for _ in 0..HEAL_MAX_ATTEMPTS {
        if !is_nodeport_type(&entry.value) || node_ports_of(&entry.value).is_empty() {
            return entry;
        }
        let Ok(listing) = store.list(&KeyPrefix::new("", "services", None)).await else {
            return entry; // give up gracefully
        };
        let foreign: BTreeSet<(String, u16)> = listing
            .iter()
            .filter(|e| e.key != key.as_path())
            .flat_map(|e| node_ports_of(&e.value))
            .collect();
        if node_ports_of(&entry.value).is_disjoint(&foreign) {
            return entry; // no collision
        }
        // Drop only the colliding ports, then re-default onto fresh ports.
        let mut next = entry.value.clone();
        for port in spec_ports_mut(&mut next) {
            let Some(Ok(n)) = explicit_node_port(port) else {
                continue;
            };
            if foreign.contains(&(protocol_of(port), n)) {
                if let Some(obj) = port.as_object_mut() {
                    obj.remove("nodePort");
                }
            }
        }
        let mut used = foreign;
        if default_service(&mut next, &mut used).is_err() {
            return entry; // cannot heal (e.g. range exhausted): keep as-is
        }
        match store.update(key, next, Some(entry.mod_revision)).await {
            Ok(updated) => return updated,
            Err(StorageError::Conflict { .. }) => {
                entry = match store.get(key).await {
                    Ok(Some(e)) => e, // someone else moved us: re-check
                    _ => return entry,
                };
            }
            Err(_) => return entry, // give up gracefully
        }
    }
    entry
}

#[cfg(test)]
mod tests {
    use super::*;
    use storage::EmbeddedStorage;

    fn svc(ty: Option<&str>, ports: Value) -> Value {
        let mut spec = json!({ "ports": ports });
        if let Some(t) = ty {
            spec["type"] = json!(t);
        }
        json!({ "apiVersion": "v1", "kind": "Service",
                "metadata": { "name": "s", "namespace": "default" }, "spec": spec })
    }

    fn node_ports(v: &Value) -> Vec<u16> {
        spec_ports(v)
            .iter()
            .filter_map(|p| explicit_node_port(p).and_then(Result::ok))
            .collect()
    }

    fn invalid(err: &ServiceError, needle: &str) -> bool {
        matches!(err, ServiceError::Invalid(m) if m.contains(needle))
    }

    #[test]
    fn default_type_becomes_clusterip() {
        let mut body = svc(None, json!([{ "port": 80 }]));
        assert!(default_service(&mut body, &mut BTreeSet::new()).is_ok());
        assert_eq!(body["spec"]["type"], "ClusterIP");
        assert!(node_ports(&body).is_empty()); // ClusterIP gets no nodePort
        let mut en = svc(Some("ExternalName"), json!([])); // untouched type
        en["spec"]["externalName"] = json!("db.example.com");
        let before = en.clone();
        assert!(default_service(&mut en, &mut BTreeSet::new()).is_ok());
        assert_eq!(en, before);
    }

    #[test]
    fn nodeport_allocation_rules() {
        let mut used = BTreeSet::from([("TCP".to_string(), NODE_PORT_MIN)]);
        let ports = json!([{ "port": 80, "targetPort": 8080 }]);
        let mut body = svc(Some("NodePort"), ports);
        assert!(default_service(&mut body, &mut used).is_ok());
        assert_eq!(node_ports(&body), vec![30001]); // lowest free
        assert!(used.contains(&("TCP".to_string(), 30001))); // marked taken
                                                             // Explicit nodePorts are honored and marked used.
        let ports = json!([{ "port": 80, "nodePort": 30080 }]);
        let mut body = svc(Some("NodePort"), ports);
        assert!(default_service(&mut body, &mut used).is_ok());
        assert_eq!(node_ports(&body), vec![30080]);
        assert!(used.contains(&("TCP".to_string(), 30080)));
        // LoadBalancer services get nodePorts too.
        let lb = json!([{ "port": 80 }]);
        let mut body = svc(Some("LoadBalancer"), lb);
        assert!(default_service(&mut body, &mut used).is_ok());
        assert_eq!(node_ports(&body), vec![30002]);
        assert_eq!(body["spec"]["type"], "LoadBalancer");
    }

    #[test]
    fn explicit_nodeport_rejections() {
        for bad in [json!(29999), json!(32768)] {
            let mut body = svc(Some("NodePort"), json!([{ "port": 80, "nodePort": bad }]));
            let err = default_service(&mut body, &mut BTreeSet::new()).unwrap_err();
            assert!(invalid(&err, "out of range"), "rejects {bad}");
        }
        for bad in [json!("30000"), json!(30000.5), json!(-1)] {
            let mut body = svc(Some("NodePort"), json!([{ "port": 80, "nodePort": bad }]));
            let err = default_service(&mut body, &mut BTreeSet::new()).unwrap_err();
            assert!(invalid(&err, "integer"), "rejects {bad}");
        }
    }

    #[test]
    fn collision_rejections() {
        // Explicit nodePort already allocated to another Service.
        let mut used = BTreeSet::from([("TCP".to_string(), 30090)]);
        let mut body = svc(Some("NodePort"), json!([{ "port": 80, "nodePort": 30090 }]));
        let err = default_service(&mut body, &mut used).unwrap_err();
        assert!(invalid(&err, "already allocated"));
        // Duplicated within one object for the same protocol.
        let ports = json!([{ "port": 80, "nodePort": 30100 }, { "port": 81, "nodePort": 30100 }]);
        let mut body = svc(Some("NodePort"), ports);
        let err = default_service(&mut body, &mut BTreeSet::new()).unwrap_err();
        assert!(invalid(&err, "duplicate"));
    }

    #[test]
    fn same_number_across_protocols_ok() {
        let ports = json!([
            { "port": 80, "protocol": "TCP", "nodePort": 30100 },
            { "port": 80, "protocol": "UDP", "nodePort": 30100 }]);
        let mut body = svc(Some("NodePort"), ports);
        let mut used = BTreeSet::new();
        assert!(default_service(&mut body, &mut used).is_ok());
        assert_eq!(node_ports(&body), vec![30100, 30100]);
        assert!(used.contains(&("UDP".to_string(), 30100)));
    }

    #[test]
    fn range_exhausted_is_invalid() {
        let mut used: BTreeSet<(String, u16)> = BTreeSet::new();
        for p in NODE_PORT_MIN..=NODE_PORT_MAX {
            used.insert(("TCP".to_string(), p));
        }
        let mut body = svc(Some("NodePort"), json!([{ "port": 80 }]));
        let err = default_service(&mut body, &mut used).unwrap_err();
        assert!(invalid(&err, "exhausted"));
    }

    #[test]
    fn used_node_ports_scans_services() {
        let np_ports = json!([
            { "port": 80, "nodePort": 30000 },
            { "port": 53, "protocol": "UDP", "nodePort": 30001 }]);
        let services = vec![
            svc(Some("NodePort"), np_ports),
            svc(
                Some("LoadBalancer"),
                json!([{ "port": 443, "nodePort": 30005 }]),
            ),
            svc(Some("ClusterIP"), json!([{ "port": 80 }])),
        ];
        let got = used_node_ports(&services);
        assert_eq!(got.len(), 3); // ClusterIP service contributes nothing
        assert!(got.contains(&("TCP".to_string(), 30000)));
        assert!(got.contains(&("UDP".to_string(), 30001)));
        assert!(got.contains(&("TCP".to_string(), 30005)));
    }

    async fn raw_create(store: &Arc<dyn StorageBackend>, name: &str, np: u16) -> StoredEntry {
        let body = svc(Some("NodePort"), json!([{ "port": 80, "nodePort": np }]));
        let key = Key::new("", "services", "default", name);
        store.create(&key, body).await.unwrap()
    }

    #[tokio::test]
    async fn heal_moves_colliding_nodeport() {
        let store: Arc<dyn StorageBackend> = Arc::new(EmbeddedStorage::new());
        let _squatter = raw_create(&store, "squatter", 30000).await;
        let ours = raw_create(&store, "ours", 30000).await;
        let key = Key::new("", "services", "default", "ours");
        let healed = heal_nodeport_collision(&store, &key, ours.clone()).await;
        assert_eq!(node_ports(&healed.value), vec![30001]); // moved
        assert!(healed.mod_revision > ours.mod_revision); // CAS advanced
        let stored = store.get(&key).await.unwrap().unwrap();
        assert_eq!(node_ports(&stored.value), vec![30001]); // persisted
    }

    #[tokio::test]
    async fn heal_passthrough_when_nothing_collides() {
        let store: Arc<dyn StorageBackend> = Arc::new(EmbeddedStorage::new());
        let _squatter = raw_create(&store, "squatter", 30000).await;
        let ours = raw_create(&store, "ours", 30001).await;
        let key = Key::new("", "services", "default", "ours");
        let healed = heal_nodeport_collision(&store, &key, ours.clone()).await;
        assert_eq!(healed, ours);
        // Non-NodePort services always pass through untouched.
        let key = Key::new("", "services", "default", "cip");
        let body = svc(Some("ClusterIP"), json!([{ "port": 80 }]));
        let entry = store.create(&key, body).await.unwrap();
        let healed = heal_nodeport_collision(&store, &key, entry.clone()).await;
        assert_eq!(healed, entry);
    }
}
