//! Argo Workflows scalar helper types (TODO **T7.3 Scope A**, decision **Q30**).
//!
//! 1:1 port of the upstream scalar wrappers: `anystring.go`, `amount.go`,
//! `item.go`, `progress.go`, `estimated_duration.go`, `label.go` and the
//! `HoldingNameVersion` / `ResourceDuration` / `ResourcesDuration` types in
//! `workflow_types.go`. Go named string/int types map to transparent Rust
//! type aliases; `AnyString` / `Amount` / `Item` are the only newtypes (the
//! spec's custom-serde set). Wire format is JSON-only (Q10).

use std::collections::BTreeMap;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// AnyString is a string type whose JSON type is just string. It will
/// unmarshal int64, int32, float64, float32, boolean, or a plain string and
/// represent it as a string. It marshals back to a string — marshalling is
/// not symmetric.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AnyString(pub String);

impl Serialize for AnyString {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for AnyString {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(deserializer)?;
        Ok(match value {
            serde_json::Value::String(s) => AnyString(s),
            serde_json::Value::Bool(b) => AnyString(b.to_string()),
            serde_json::Value::Number(n) => AnyString(format_number(&n)),
            // Upstream's type switch leaves the value untouched (empty) for
            // any other JSON value — mirror that instead of erroring.
            _ => AnyString::default(),
        })
    }
}

/// Format a JSON number the way Go's `AnyString.UnmarshalJSON` does:
/// integers in decimal, floats via shortest round-trip fixed-point
/// (`strconv.FormatFloat(v, 'f', -1, 64)` — never scientific notation,
/// so `42.0` becomes `"42"` and `3.5` becomes `"3.5"`).
fn format_number(n: &serde_json::Number) -> String {
    if let Some(i) = n.as_i64() {
        i.to_string()
    } else if let Some(u) = n.as_u64() {
        u.to_string()
    } else if let Some(f) = n.as_f64() {
        format!("{f}")
    } else {
        n.to_string()
    }
}

/// Amount represents a numeric amount. Upstream stores the raw `json.Number`;
/// the transparent `serde_json::Value` newtype (plain derive) gives identical
/// raw-JSON number passthrough on the wire.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Amount(pub serde_json::Value);

/// Item expands a single workflow step into multiple parallel steps. The
/// value of Item can be a map, string, bool, or number — a transparent
/// `serde_json::Value` newtype (plain derive) passes the union through.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Item(pub serde_json::Value);

/// Progress in N/M format. N is number of task complete. M is number of tasks.
pub type Progress = String;

/// Undefined progress (zero value).
pub const PROGRESS_UNDEFINED: &str = "";
/// Zero value (not the same as "no progress").
pub const PROGRESS_ZERO: &str = "0/0";
/// Default progress.
pub const PROGRESS_DEFAULT: &str = "0/1";

/// EstimatedDuration is in seconds (Go `int`, modelled as `i64`).
pub type EstimatedDuration = i64;

/// ResourceDuration is the amount of requested resource * the duration that
/// request was used, represented as seconds (Go `int64`).
pub type ResourceDuration = i64;

/// ResourcesDuration contains each duration by resource requested,
/// e.g. 100m CPU * 1h, 1Gi memory * 1h (Go map keyed by resource name).
pub type ResourcesDuration = BTreeMap<String, ResourceDuration>;

/// LabelValues is a list of workflow labels.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct LabelValues {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<String>,
}

/// LabelKeys is list of keys.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct LabelKeys {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<String>,
}

/// HoldingNameVersion is the schema version of a semaphore/mutex holding
/// (Go `int` enum — `HoldingNameV1` named keys, `HoldingNameV2` indexed keys).
pub type HoldingNameVersion = i32;

/// Holding keyed by name.
pub const HOLDING_NAME_V1: HoldingNameVersion = 1;
/// Holding keyed by index.
pub const HOLDING_NAME_V2: HoldingNameVersion = 2;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn any_string_from_json_number() {
        let v: AnyString = serde_json::from_str("42").unwrap();
        assert_eq!(v, AnyString("42".to_string()));
        assert_eq!(serde_json::to_string(&v).unwrap(), "\"42\"");
    }

    #[test]
    fn any_string_from_json_bool() {
        let v: AnyString = serde_json::from_str("true").unwrap();
        assert_eq!(v.0, "true");
        assert_eq!(serde_json::to_string(&v).unwrap(), "\"true\"");
    }

    #[test]
    fn any_string_from_json_float() {
        let v: AnyString = serde_json::from_str("3.5").unwrap();
        assert_eq!(v.0, "3.5");
        // Go formats float64(42.0) via shortest round-trip: "42".
        let w: AnyString = serde_json::from_str("42.0").unwrap();
        assert_eq!(w.0, "42");
        assert_eq!(serde_json::to_string(&w).unwrap(), "\"42\"");
    }

    #[test]
    fn any_string_serializes_as_plain_string() {
        let v: AnyString = serde_json::from_str("\"hello\"").unwrap();
        assert_eq!(v.0, "hello");
        assert_eq!(serde_json::to_string(&v).unwrap(), "\"hello\"");
    }

    #[test]
    fn item_passthrough() {
        let map: Item = serde_json::from_str(r#"{"a":1,"b":"x"}"#).unwrap();
        assert_eq!(
            serde_json::to_value(&map).unwrap(),
            serde_json::json!({"a": 1, "b": "x"})
        );
        let s: Item = serde_json::from_str(r#""plain""#).unwrap();
        assert_eq!(serde_json::to_string(&s).unwrap(), "\"plain\"");
    }

    #[test]
    fn amount_passthrough() {
        let a: Amount = serde_json::from_str("1.25").unwrap();
        assert_eq!(serde_json::to_string(&a).unwrap(), "1.25");
    }
}
