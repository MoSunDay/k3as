//! `init-pro.io/v1` `TrafficSplit` — weighted one-click traffic switching
//! (TODO **T8.2**, decision **Q57**).
//!
//! A split applies to a *NodePort entry Service* (the listener owner): its
//! `spec.backends[]` name backend Services and relative weights. The Router
//! folds splits into its resolver state, plans per-endpoint weights, and
//! hot-reloads the openrusty upstream. `weight: 0` disables a backend
//! (openrusty rejects zero-weight peers, so zero backends are dropped).
//!
//! The type is a plain Kubernetes object: flattened `TypeMeta`, `ObjectMeta`,
//! typed spec, optional status (JSON-only wire format, Q10).

use kube_core::gvk::GroupVersionKind;
use kube_core::metadata::{ListMeta, ObjectMeta, TypeMeta};
use serde::{Deserialize, Serialize};

use crate::initpro::Condition;
use crate::schema::{SchemaRegistry, Scope};

/// API group + version (shared by every init-pro.io resource).
pub const GROUP: &str = "init-pro.io";
pub const VERSION: &str = "v1";
pub const API_VERSION: &str = "init-pro.io/v1";

/// Kind / plural / short name.
pub const KIND: &str = "TrafficSplit";
pub const PLURAL: &str = "trafficsplits";
pub const SHORT: &str = "ts";

/// `TrafficSplit` — weighted backend set for one entry Service.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct TrafficSplit {
    #[serde(flatten)]
    pub types: TypeMeta,
    pub metadata: ObjectMeta,
    pub spec: TrafficSplitSpec,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub status: Option<TrafficSplitStatus>,
}

/// Desired split state.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TrafficSplitSpec {
    /// The NodePort entry Service (listener owner), `name` or `namespace/name`.
    pub service: String,
    /// Backend Services with relative weights; `0` = disabled.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub backends: Vec<SplitBackend>,
}

/// One backend Service + its relative weight.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct SplitBackend {
    /// Backend Service name (`name` or `namespace/name`).
    pub service: String,
    /// Relative weight; `<= 0` drops the backend (Q57).
    #[serde(default)]
    pub weight: i32,
}

/// Observed split state (the Router compiles the split; the status records
/// the applied active backend count).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TrafficSplitStatus {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<Condition>,
    /// Backends with `weight > 0` at the last compile.
    #[serde(default, skip_serializing_if = "is_zero_i32")]
    pub active_backends: i32,
}

/// List wrapper.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct TrafficSplitList {
    #[serde(flatten)]
    pub types: TypeMeta,
    #[serde(default)]
    pub metadata: ListMeta,
    #[serde(default)]
    pub items: Vec<TrafficSplit>,
}

impl TrafficSplitSpec {
    /// Active backends only (`weight > 0`), in declaration order (Q57).
    pub fn active_backends(&self) -> impl Iterator<Item = &SplitBackend> {
        self.backends.iter().filter(|b| b.weight > 0)
    }

    /// Sum of active weights (0 = every backend disabled).
    pub fn total_weight(&self) -> i32 {
        self.active_backends().map(|b| b.weight).sum()
    }
}

fn is_zero_i32(v: &i32) -> bool {
    *v == 0
}

/// Register the `TrafficSplit` type (short name `ts`).
pub fn register(registry: &mut SchemaRegistry) {
    registry.register_with_short_names(
        GROUP,
        VERSION,
        KIND,
        PLURAL,
        "TrafficSplitList",
        Scope::Namespaced,
        &[SHORT],
    );
}

/// The GVK for `TrafficSplit` (convenience).
pub fn gvk() -> GroupVersionKind {
    GroupVersionKind::gvk(GROUP, VERSION, KIND)
}

/// Construct a `TrafficSplit` with correct `TypeMeta`.
pub fn new_traffic_split(name: &str, namespace: &str, spec: TrafficSplitSpec) -> TrafficSplit {
    TrafficSplit {
        types: TypeMeta {
            api_version: API_VERSION.to_string(),
            kind: KIND.to_string(),
        },
        metadata: ObjectMeta {
            name: Some(name.to_string()),
            namespace: Some(namespace.to_string()),
            ..ObjectMeta::default()
        },
        spec,
        status: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::serde_ext::round_trip;

    fn split() -> TrafficSplit {
        new_traffic_split(
            "code-grapher",
            "default",
            TrafficSplitSpec {
                service: "code-grapher".into(),
                backends: vec![
                    SplitBackend {
                        service: "code-grapher-stable".into(),
                        weight: 90,
                    },
                    SplitBackend {
                        service: "code-grapher-canary".into(),
                        weight: 10,
                    },
                ],
            },
        )
    }

    #[test]
    fn register_and_resolve_trafficsplit() {
        let mut reg = SchemaRegistry::new();
        register(&mut reg);
        let info = reg.get(&gvk()).expect("TrafficSplit registered");
        assert_eq!(info.kind, "TrafficSplit");
        assert_eq!(info.resource, "trafficsplits");
        assert_eq!(info.scope, Scope::Namespaced);
        assert!(info.short_names.iter().any(|s| s == "ts"));
    }

    #[test]
    fn round_trip_is_lossless() {
        let s = split();
        assert_eq!(round_trip(&s).unwrap(), s);
    }

    #[test]
    fn camel_case_on_the_wire() {
        let json = serde_json::to_string(&split()).unwrap();
        assert!(json.contains("\"apiVersion\":\"init-pro.io/v1\""));
        assert!(json.contains("\"kind\":\"TrafficSplit\""));
        assert!(json.contains("\"service\":\"code-grapher-stable\""));
        assert!(json.contains("\"weight\":90"));
    }

    #[test]
    fn zero_weight_backends_are_inactive() {
        let s = TrafficSplitSpec {
            service: "app".into(),
            backends: vec![
                SplitBackend {
                    service: "a".into(),
                    weight: 0,
                },
                SplitBackend {
                    service: "b".into(),
                    weight: 7,
                },
            ],
        };
        assert_eq!(s.active_backends().count(), 1);
        assert_eq!(s.total_weight(), 7);
    }
}
