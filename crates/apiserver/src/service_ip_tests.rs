//! Unit tests for the ClusterIP allocation plane (`service_ip.rs`,
//! T4.3 Scope C / Q44) + its S52-D0 write-path admission
//! (`validate_on_write`, immutable-or-validate) — split out to keep that
//! file under the 400-line cap (the `authz_tests` / `pod_io_tests`
//! precedent).

use crate::service::ServiceError;
use crate::service_ip::*;
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::sync::Arc;
use storage::{EmbeddedStorage, Key, StorageBackend, StoredEntry};

fn svc(ty: Option<&str>, spec: Value) -> Value {
    let mut spec = spec;
    if let Some(t) = ty {
        spec["type"] = json!(t);
    }
    json!({ "apiVersion": "v1", "kind": "Service",
            "metadata": { "name": "s", "namespace": "default" }, "spec": spec })
}

fn cluster_ip(v: &Value) -> Option<String> {
    v.pointer("/spec/clusterIP")
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn invalid(err: &ServiceError, needle: &str) -> bool {
    matches!(err, ServiceError::Invalid(m) if m.contains(needle))
}

fn default_cidr() -> ServiceCidr {
    ServiceCidr::parse(DEFAULT_SERVICE_CIDR).unwrap()
}

#[test]
fn cidr_parse_display_contains() {
    let c = default_cidr();
    assert_eq!(c.to_string(), "10.43.0.0/16");
    assert!(c.contains("10.43.0.1".parse().unwrap()));
    assert!(c.contains("10.43.255.254".parse().unwrap()));
    assert!(!c.contains("10.44.0.1".parse().unwrap()));
    assert!(!c.contains("10.42.255.255".parse().unwrap()));
    // host bits are masked off at parse (10.43.5.9/16 -> 10.43.0.0/16)
    assert_eq!(ServiceCidr::parse("10.43.5.9/16"), Some(c));
    // rejects garbage, /33, IPv6, short quads
    for bad in [
        "10.43.0.0",
        "10.43.0.0/33",
        "10.43.0.0/x",
        "fd00::/16",
        "10.43/16",
        "300.1.1.1/8",
        "10.43.0.-1/16",
        "",
    ] {
        assert!(ServiceCidr::parse(bad).is_none(), "{bad} rejected");
    }
    // /0 contains everything; /32 contains only itself.
    let all = ServiceCidr::parse("0.0.0.0/0").unwrap();
    assert!(all.contains("192.168.1.1".parse().unwrap()));
    let one = ServiceCidr::parse("10.0.0.1/32").unwrap();
    assert!(one.contains("10.0.0.1".parse().unwrap()));
    assert!(!one.contains("10.0.0.2".parse().unwrap()));
    // Default equals the parsed k3s default.
    assert_eq!(ServiceCidr::default(), default_cidr());
}

#[test]
fn lowest_free_sequential_allocation() {
    let cidr = default_cidr();
    let mut used = BTreeSet::new();
    // First: 10.43.0.1 (network .0 skipped), second: 10.43.0.2.
    let mut a = svc(None, json!({ "ports": [{ "port": 80 }] }));
    assert!(default_cluster_ip(&mut a, &mut used, &cidr).is_ok());
    assert_eq!(cluster_ip(&a).as_deref(), Some("10.43.0.1"));
    let mut b = svc(None, json!({ "ports": [{ "port": 80 }] }));
    assert!(default_cluster_ip(&mut b, &mut used, &cidr).is_ok());
    assert_eq!(cluster_ip(&b).as_deref(), Some("10.43.0.2"));
    assert_ne!(cluster_ip(&a), cluster_ip(&b), "distinct across services");
    // A hole is filled lowest-first.
    let mut used = BTreeSet::from(["10.43.0.1".parse().unwrap(), "10.43.0.2".parse().unwrap()]);
    let mut c = svc(None, json!({ "ports": [{ "port": 80 }] }));
    assert!(default_cluster_ip(&mut c, &mut used, &cidr).is_ok());
    assert_eq!(cluster_ip(&c).as_deref(), Some("10.43.0.3"));
}

#[test]
fn explicit_in_cidr_honored_and_marked_used() {
    let cidr = default_cidr();
    let mut used = BTreeSet::new();
    let mut body = svc(
        Some("ClusterIP"),
        json!({ "ports": [{ "port": 80 }], "clusterIP": "10.43.7.7" }),
    );
    assert!(default_cluster_ip(&mut body, &mut used, &cidr).is_ok());
    assert_eq!(cluster_ip(&body).as_deref(), Some("10.43.7.7"), "verbatim");
    // The explicit IP is now taken for the next service.
    let mut other = svc(
        None,
        json!({ "ports": [{ "port": 80 }], "clusterIP": "10.43.7.7" }),
    );
    let err = default_cluster_ip(&mut other, &mut used, &cidr).unwrap_err();
    assert!(invalid(&err, "already allocated"), "{err:?}");
}

#[test]
fn explicit_out_of_cidr_rejected() {
    let cidr = default_cidr();
    for bad in ["10.44.0.5", "192.168.1.1", "10.42.0.1", "10.0.0.1"] {
        let mut body = svc(
            Some("ClusterIP"),
            json!({ "ports": [{ "port": 80 }], "clusterIP": bad }),
        );
        let err = default_cluster_ip(&mut body, &mut BTreeSet::new(), &cidr).unwrap_err();
        assert!(
            invalid(&err, bad) && invalid(&err, "outside"),
            "{bad}: {err:?}"
        );
    }
}

#[test]
fn garbage_clusterip_rejected() {
    let cidr = default_cidr();
    for bad in ["fd00::1", "not-an-ip", "10.43", ""] {
        let mut body = svc(
            Some("ClusterIP"),
            json!({ "ports": [{ "port": 80 }], "clusterIP": bad }),
        );
        let err = default_cluster_ip(&mut body, &mut BTreeSet::new(), &cidr).unwrap_err();
        assert!(invalid(&err, "not a valid IPv4"), "{bad}: {err:?}");
    }
}

#[test]
fn headless_and_externalname_untouched() {
    let cidr = default_cidr();
    // clusterIP "None" stays verbatim, nothing allocated.
    let mut headless = svc(
        Some("ClusterIP"),
        json!({ "ports": [{ "port": 80 }], "clusterIP": "None" }),
    );
    let before = headless.clone();
    assert!(default_cluster_ip(&mut headless, &mut BTreeSet::new(), &cidr).is_ok());
    assert_eq!(headless, before, "headless untouched");
    // ExternalName: no clusterIP injected even when absent.
    let mut en = svc(
        Some("ExternalName"),
        json!({ "externalName": "db.example.com" }),
    );
    let before = en.clone();
    assert!(default_cluster_ip(&mut en, &mut BTreeSet::new(), &cidr).is_ok());
    assert_eq!(en, before, "ExternalName untouched");
    // Absent ports: no allocation either.
    let mut bare = svc(None, json!({}));
    let before = bare.clone();
    assert!(default_cluster_ip(&mut bare, &mut BTreeSet::new(), &cidr).is_ok());
    assert_eq!(bare, before, "portless untouched");
}

#[test]
fn used_cluster_ips_scans_services() {
    let services = vec![
        svc(
            Some("ClusterIP"),
            json!({ "ports": [{ "port": 80 }], "clusterIP": "10.43.0.5" }),
        ),
        svc(
            Some("ClusterIP"),
            json!({ "ports": [{ "port": 80 }], "clusterIP": "None" }),
        ),
        svc(Some("ClusterIP"), json!({ "ports": [{ "port": 80 }] })), // not yet allocated
        svc(
            Some("ExternalName"),
            json!({ "externalName": "x", "clusterIP": "10.43.0.9" }),
        ),
    ];
    let got = used_cluster_ips(&services);
    assert_eq!(got.len(), 2, "parseable IPs only: {got:?}");
    assert!(got.contains(&"10.43.0.5".parse().unwrap()));
    assert!(got.contains(&"10.43.0.9".parse().unwrap()));
}

#[test]
fn slash31_and_slash32_allocate_every_address() {
    // /31: both addresses; /32: the single address.
    let mut used = BTreeSet::new();
    let c31 = ServiceCidr::parse("10.43.0.0/31").unwrap();
    let mut a = svc(None, json!({ "ports": [{ "port": 80 }] }));
    assert!(default_cluster_ip(&mut a, &mut used, &c31).is_ok());
    assert_eq!(cluster_ip(&a).as_deref(), Some("10.43.0.0"));
    let mut b = svc(None, json!({ "ports": [{ "port": 80 }] }));
    assert!(default_cluster_ip(&mut b, &mut used, &c31).is_ok());
    assert_eq!(cluster_ip(&b).as_deref(), Some("10.43.0.1"));
    let c32 = ServiceCidr::parse("10.43.0.1/32").unwrap();
    let mut d = svc(None, json!({ "ports": [{ "port": 80 }] }));
    assert!(default_cluster_ip(&mut d, &mut BTreeSet::new(), &c32).is_ok());
    assert_eq!(cluster_ip(&d).as_deref(), Some("10.43.0.1"));
}

async fn raw_create(store: &Arc<dyn StorageBackend>, name: &str, ip: &str) -> StoredEntry {
    let body = svc(
        Some("ClusterIP"),
        json!({ "ports": [{ "port": 80 }], "clusterIP": ip }),
    );
    let key = Key::new("", "services", "default", name);
    store.create(&key, body).await.unwrap()
}

#[tokio::test]
async fn heal_moves_colliding_ip() {
    let store: Arc<dyn StorageBackend> = Arc::new(EmbeddedStorage::new());
    let _squatter = raw_create(&store, "squatter", "10.43.0.1").await;
    let ours = raw_create(&store, "ours", "10.43.0.1").await;
    let key = Key::new("", "services", "default", "ours");
    let healed = heal_clusterip_collision(&store, &key, ours.clone(), &default_cidr()).await;
    assert_eq!(cluster_ip(&healed.value).as_deref(), Some("10.43.0.2")); // moved
    assert!(healed.mod_revision > ours.mod_revision); // CAS advanced
    let stored = store.get(&key).await.unwrap().unwrap();
    assert_eq!(cluster_ip(&stored.value).as_deref(), Some("10.43.0.2")); // persisted
}

#[tokio::test]
async fn heal_passthrough_when_nothing_collides() {
    let store: Arc<dyn StorageBackend> = Arc::new(EmbeddedStorage::new());
    let _other = raw_create(&store, "other", "10.43.0.1").await;
    let ours = raw_create(&store, "ours", "10.43.0.2").await;
    let key = Key::new("", "services", "default", "ours");
    let healed = heal_clusterip_collision(&store, &key, ours.clone(), &default_cidr()).await;
    assert_eq!(healed, ours, "no collision: untouched");
    // Headless "None" and malformed values pass through untouched too.
    let key = Key::new("", "services", "default", "headless");
    let body = svc(
        Some("ClusterIP"),
        json!({ "ports": [{ "port": 80 }], "clusterIP": "None" }),
    );
    let entry = store.create(&key, body).await.unwrap();
    let healed = heal_clusterip_collision(&store, &key, entry.clone(), &default_cidr()).await;
    assert_eq!(healed, entry, "headless untouched");
    let key = Key::new("", "services", "default", "junk");
    let body = svc(
        Some("ClusterIP"),
        json!({ "ports": [{ "port": 80 }], "clusterIP": "junk" }),
    );
    let entry = store.create(&key, body).await.unwrap();
    let healed = heal_clusterip_collision(&store, &key, entry.clone(), &default_cidr()).await;
    assert_eq!(healed, entry, "malformed untouched");
}

// --- S52-D0 (T4.3 Scope C follow-up / Q44): validate_on_write ------------

/// A fresh store with `name -> body` Services pre-seeded in `default`.
async fn write_store(seed: Vec<(&str, Value)>) -> Arc<dyn StorageBackend> {
    let store: Arc<dyn StorageBackend> = Arc::new(EmbeddedStorage::new());
    for (name, body) in seed {
        store
            .create(&Key::new("", "services", "default", name), body)
            .await
            .unwrap();
    }
    store
}

fn our_key() -> Key {
    Key::new("", "services", "default", "ours")
}

/// A stored/put body with an explicit clusterIP + nodePort-bearing ports.
fn full_svc(cluster_ip: &str, node_port: u16) -> Value {
    svc(
        Some("NodePort"),
        json!({ "ports": [{ "port": 80, "nodePort": node_port }], "clusterIP": cluster_ip }),
    )
}

#[tokio::test]
async fn write_clusterip_change_rejected() {
    let old = full_svc("10.43.0.9", 30100);
    let store = write_store(vec![("ours", old.clone())]).await;
    let mut new = full_svc("10.43.0.10", 30100);
    let err = validate_on_write(&store, &our_key(), Some(&old), &mut new, &default_cidr())
        .await
        .unwrap_err();
    assert!(invalid(&err, "may not change once set"), "{err:?}");
}

#[tokio::test]
async fn write_clusterip_drop_defaults_from_stored() {
    // k8s update parity: an omitted clusterIP persists the allocation
    // (defaulted from the stored object), it is not a rejection.
    let old = full_svc("10.43.0.9", 30100);
    let store = write_store(vec![("ours", old.clone())]).await;
    let mut new = svc(
        Some("NodePort"),
        json!({ "ports": [{ "port": 80, "nodePort": 30100 }] }),
    );
    validate_on_write(&store, &our_key(), Some(&old), &mut new, &default_cidr())
        .await
        .expect("omitted clusterIP defaults from the stored object");
    assert_eq!(
        new.pointer("/spec/clusterIP").and_then(Value::as_str),
        Some("10.43.0.9"),
        "the stored clusterIP is written back: {new}"
    );
}

#[tokio::test]
async fn write_clusterip_unchanged_allowed_without_reallocation() {
    let old = full_svc("10.43.0.9", 30100);
    let store = write_store(vec![("ours", old.clone())]).await;
    let mut new = full_svc("10.43.0.9", 30100);
    let before = new.clone();
    assert!(
        validate_on_write(&store, &our_key(), Some(&old), &mut new, &default_cidr())
            .await
            .is_ok()
    );
    assert_eq!(new, before, "unchanged write must not mutate the object");
}

#[tokio::test]
async fn write_clusterip_headless_none_is_immutable_too() {
    let old = svc(
        Some("ClusterIP"),
        json!({ "ports": [{ "port": 80 }], "clusterIP": "None" }),
    );
    let store = write_store(vec![("ours", old.clone())]).await;
    // Keeping "None" is fine.
    let mut same = old.clone();
    assert!(
        validate_on_write(&store, &our_key(), Some(&old), &mut same, &default_cidr())
            .await
            .is_ok()
    );
    // Escaping headless is a change.
    let mut vip = full_svc("10.43.0.5", 30100);
    let err = validate_on_write(&store, &our_key(), Some(&old), &mut vip, &default_cidr())
        .await
        .unwrap_err();
    assert!(invalid(&err, "may not change once set"), "{err:?}");
}

#[tokio::test]
async fn write_clusterip_unset_old_uses_create_judgment() {
    // Old carries no clusterIP (pre-Q44 legacy object): the incoming one is
    // judged exactly like the create path.
    let old = svc(Some("ClusterIP"), json!({ "ports": [{ "port": 80 }] }));
    let squatter = svc(
        Some("ClusterIP"),
        json!({ "ports": [{ "port": 80 }], "clusterIP": "10.43.0.9" }),
    );
    let store = write_store(vec![("ours", old.clone()), ("squatter", squatter)]).await;
    // Taken by a DIFFERENT Service -> cross-Service conflict (a shared
    // VIP implies a shared VIP:port pair).
    let mut taken = svc(
        Some("ClusterIP"),
        json!({ "ports": [{ "port": 80 }], "clusterIP": "10.43.0.9" }),
    );
    let err = validate_on_write(&store, &our_key(), Some(&old), &mut taken, &default_cidr())
        .await
        .unwrap_err();
    assert!(invalid(&err, "already allocated"), "{err:?}");
    // Out of CIDR -> rejected with the create-path message.
    let mut outside = svc(
        Some("ClusterIP"),
        json!({ "ports": [{ "port": 80 }], "clusterIP": "10.44.0.1" }),
    );
    let err = validate_on_write(
        &store,
        &our_key(),
        Some(&old),
        &mut outside,
        &default_cidr(),
    )
    .await
    .unwrap_err();
    assert!(invalid(&err, "outside"), "{err:?}");
    // Free + in CIDR -> kept verbatim; absent -> allocated in place.
    let mut free = svc(
        Some("ClusterIP"),
        json!({ "ports": [{ "port": 80 }], "clusterIP": "10.43.3.3" }),
    );
    assert!(
        validate_on_write(&store, &our_key(), Some(&old), &mut free, &default_cidr())
            .await
            .is_ok()
    );
    assert_eq!(cluster_ip(&free).as_deref(), Some("10.43.3.3"));
    let mut absent = svc(Some("ClusterIP"), json!({ "ports": [{ "port": 80 }] }));
    assert!(
        validate_on_write(&store, &our_key(), Some(&old), &mut absent, &default_cidr())
            .await
            .is_ok()
    );
    assert_eq!(
        cluster_ip(&absent).as_deref(),
        Some("10.43.0.1"),
        "lowest free"
    );
}

#[tokio::test]
async fn write_clusterip_absent_old_none_allocates_like_create() {
    // The create-via-apply branch: no stored object at all.
    let store = write_store(vec![]).await;
    let mut new = svc(Some("ClusterIP"), json!({ "ports": [{ "port": 80 }] }));
    assert!(
        validate_on_write(&store, &our_key(), None, &mut new, &default_cidr())
            .await
            .is_ok()
    );
    assert_eq!(cluster_ip(&new).as_deref(), Some("10.43.0.1"));
}

#[tokio::test]
async fn write_nodeport_change_and_drop_rejected() {
    let old = full_svc("10.43.0.9", 30100);
    let store = write_store(vec![("ours", old.clone())]).await;
    // Changed.
    let mut changed = full_svc("10.43.0.9", 30101);
    let err = validate_on_write(
        &store,
        &our_key(),
        Some(&old),
        &mut changed,
        &default_cidr(),
    )
    .await
    .unwrap_err();
    assert!(invalid(&err, "30100"), "{err:?}");
    assert!(invalid(&err, "may not change once set"), "{err:?}");
    // Dropped (port entry without a nodePort): the allocation persists
    // (k8s update parity) — the stored nodePort is defaulted back in.
    let mut dropped = svc(
        Some("NodePort"),
        json!({ "ports": [{ "port": 80 }], "clusterIP": "10.43.0.9" }),
    );
    validate_on_write(
        &store,
        &our_key(),
        Some(&old),
        &mut dropped,
        &default_cidr(),
    )
    .await
    .expect("omitted nodePort persists the stored allocation");
    assert_eq!(
        dropped
            .pointer("/spec/ports/0/nodePort")
            .and_then(Value::as_u64),
        Some(30100),
        "the stored nodePort is written back: {dropped}"
    );
}

#[tokio::test]
async fn write_nodeport_unchanged_and_additions_ok() {
    let old = full_svc("10.43.0.9", 30100);
    let store = write_store(vec![("ours", old.clone())]).await;
    // Unchanged.
    let mut same = full_svc("10.43.0.9", 30100);
    assert!(
        validate_on_write(&store, &our_key(), Some(&old), &mut same, &default_cidr())
            .await
            .is_ok()
    );
    // A NEW port entry gets the create-path judgment (allocate-if-absent).
    let mut grown = svc(
        Some("NodePort"),
        json!({
            "ports": [
                { "port": 80, "nodePort": 30100 },
                { "port": 81 }
            ],
            "clusterIP": "10.43.0.9"
        }),
    );
    assert!(
        validate_on_write(&store, &our_key(), Some(&old), &mut grown, &default_cidr())
            .await
            .is_ok()
    );
    assert_eq!(
        grown
            .pointer("/spec/ports/1/nodePort")
            .and_then(Value::as_u64),
        Some(30000),
        "new port allocated lowest free"
    );
}

#[tokio::test]
async fn write_nodeport_unset_old_validates_incoming() {
    // Old holds no nodePort pairs (ClusterIP-typed): the incoming nodePort
    // runs the create-path judgment, cross-Service conflicts included.
    let old = svc(
        Some("ClusterIP"),
        json!({ "ports": [{ "port": 80 }], "clusterIP": "10.43.0.9" }),
    );
    let squatter = svc(
        Some("NodePort"),
        json!({ "ports": [{ "port": 80, "nodePort": 30090 }] }),
    );
    let store = write_store(vec![("ours", old.clone()), ("squatter", squatter)]).await;
    let mut conflict = svc(
        Some("NodePort"),
        json!({ "ports": [{ "port": 80, "nodePort": 30090 }], "clusterIP": "10.43.0.9" }),
    );
    let err = validate_on_write(
        &store,
        &our_key(),
        Some(&old),
        &mut conflict,
        &default_cidr(),
    )
    .await
    .unwrap_err();
    assert!(invalid(&err, "already allocated"), "{err:?}");
    let mut free = svc(
        Some("NodePort"),
        json!({ "ports": [{ "port": 80, "nodePort": 30123 }], "clusterIP": "10.43.0.9" }),
    );
    assert!(
        validate_on_write(&store, &our_key(), Some(&old), &mut free, &default_cidr())
            .await
            .is_ok()
    );
    assert_eq!(node_port_of(&free), Some(30123), "valid explicit kept");
}

/// First `spec.ports[].nodePort` of a body as `u16`.
fn node_port_of(v: &Value) -> Option<u16> {
    v.pointer("/spec/ports/0/nodePort")
        .and_then(Value::as_u64)
        .and_then(|n| u16::try_from(n).ok())
}
