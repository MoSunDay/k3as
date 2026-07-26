//! JSON round-trip fidelity for representative Kubernetes objects (T1.1, S4).
//!
//! Decision Q10: JSON is the sole wire format. Each fixture is decoded into its
//! typed k8s-openapi struct, re-serialized, and asserted to be:
//!   (1) semantically equal (canonical Value compare), and
//!   (2) idempotent (serialize twice -> identical bytes), proving losslessness.

use init_pro_api::serde_ext::{canonical_json, from_json, to_json};
use k8s_openapi::api::core::v1::{ConfigMap, Namespace, Pod};

/// Assert a typed round-trip is lossless: bytes -> T -> bytes' -> bytes''.
/// `bytes'` and `bytes''` must be byte-identical (idempotent serialization),
/// and canonical(bytes) == canonical(input).
fn assert_fidelity<T>(input: &[u8])
where
    T: serde::de::DeserializeOwned + serde::Serialize,
{
    let obj: T = from_json(input).expect("decode fixture");
    let once = to_json(&obj).expect("encode once");
    let twice = {
        let again: T = from_json(&once).expect("re-decode");
        to_json(&again).expect("encode twice")
    };
    // idempotent: second serialization == first
    assert_eq!(once, twice, "serialization must be idempotent (stable field order)");

    // semantically lossless: canonical form of input == canonical of output
    assert_eq!(
        canonical_json(input).expect("canonical input"),
        canonical_json(&once).expect("canonical output"),
        "round-trip must be semantically lossless"
    );
}

const NAMESPACE_JSON: &str = r#"{
  "apiVersion": "v1",
  "kind": "Namespace",
  "metadata": {
    "name": "staging",
    "labels": {"env": "staging", "tier": "frontend"},
    "annotations": {"managed-by": "init-pro"}
  },
  "spec": {"finalizers": ["kubernetes"]},
  "status": {"phase": "Active"}
}"#;

const CONFIGMAP_JSON: &str = r#"{
  "apiVersion": "v1",
  "kind": "ConfigMap",
  "metadata": {"name": "app-config", "namespace": "default"},
  "data": {
    "application.yml": "server:\n  port: 8080\n",
    "LOG_LEVEL": "debug"
  },
  "binaryData": {"blob": "aGVsbG8="}
}"#;

const POD_JSON: &str = r#"{
  "apiVersion": "v1",
  "kind": "Pod",
  "metadata": {
    "name": "nginx",
    "namespace": "default",
    "labels": {"app": "nginx", "tier": "web"},
    "annotations": {"prometheus.io/scrape": "true"}
  },
  "spec": {
    "containers": [
      {
        "name": "nginx",
        "image": "nginx:1.27",
        "ports": [{"containerPort": 80, "protocol": "TCP"}],
        "env": [{"name": "NGINX_HOST", "value": "localhost"}],
        "resources": {"requests": {"cpu": "100m", "memory": "128Mi"}}
      }
    ],
    "volumes": [{"name": "cache", "emptyDir": {}}]
  },
  "status": {"phase": "Running", "podIP": "10.0.0.5"}
}"#;

#[test]
fn namespace_round_trip_is_lossless() {
    assert_fidelity::<Namespace>(NAMESPACE_JSON.as_bytes());
}

#[test]
fn configmap_round_trip_is_lossless() {
    assert_fidelity::<ConfigMap>(CONFIGMAP_JSON.as_bytes());
}

#[test]
fn pod_round_trip_is_lossless() {
    assert_fidelity::<Pod>(POD_JSON.as_bytes());
}

#[test]
fn pod_metadata_name_survives_round_trip() {
    let pod: Pod = from_json(POD_JSON.as_bytes()).unwrap();
    assert_eq!(pod.metadata.name.as_deref(), Some("nginx"));
    assert_eq!(pod.metadata.namespace.as_deref(), Some("default"));
    assert_eq!(pod.spec.as_ref().unwrap().containers.len(), 1);
    // re-encode and check the container image is intact
    let bytes = to_json(&pod).unwrap();
    let again: Pod = from_json(&bytes).unwrap();
    assert_eq!(
        again.spec.as_ref().unwrap().containers[0].image.as_deref(),
        Some("nginx:1.27")
    );
}

#[test]
fn configmap_data_key_order_does_not_affect_equality() {
    // Two semantically-identical ConfigMaps with differently-ordered keys
    // must canonicalize to the same bytes.
    let a = r#"{"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"c"},"data":{"b":"2","a":"1"}}"#;
    let b = r#"{"data":{"a":"1","b":"2"},"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"c"}}"#;
    assert_eq!(
        canonical_json(a.as_bytes()).unwrap(),
        canonical_json(b.as_bytes()).unwrap()
    );
}
