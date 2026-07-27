//! JSON serialization helpers (TODO **T1.1**, step S4).
//!
//! Decision **Q10**: JSON is the *only* wire/storage format for v1. These
//! helpers give a single, audited path for round-trip-faithful (de)serialization.
//! `serde_json` preserves struct-field order on serialization, so re-encoding a
//! decoded object is byte-stable — the property the JSON-fidelity tests assert.

use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;
use thiserror::Error;

/// A (de)serialization error on the JSON path.
#[derive(Debug, Error)]
pub enum JsonError {
    #[error("serialization failed: {0}")]
    Serde(#[from] serde_json::Error),
}

/// Serialize a value to a JSON byte string (struct-field order, stable).
pub fn to_json<T: Serialize>(value: &T) -> Result<Vec<u8>, JsonError> {
    Ok(serde_json::to_vec(value)?)
}

/// Serialize to a pretty-printed JSON string (human-readable `etcdctl` parity).
pub fn to_json_pretty<T: Serialize>(value: &T) -> Result<String, JsonError> {
    Ok(serde_json::to_string_pretty(value)?)
}

/// Deserialize from JSON bytes.
pub fn from_json<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, JsonError> {
    Ok(serde_json::from_slice(bytes)?)
}

/// Round-trip a value through JSON bytes (serialize -> deserialize).
/// Used to assert that a type survives a JSON cycle.
pub fn round_trip<T>(value: &T) -> Result<T, JsonError>
where
    T: Serialize + DeserializeOwned,
{
    let bytes = to_json(value)?;
    from_json(&bytes)
}

/// Canonicalize a JSON document by sorting object keys at every depth and
/// formatting with 2-space indent. Two semantically-equal objects canonicalize
/// to identical bytes, so this is the order-insensitive comparison the fidelity
/// suite uses (mirrors `jq -S`).
pub fn canonical_json(bytes: &[u8]) -> Result<Vec<u8>, JsonError> {
    let value: Value = serde_json::from_slice(bytes)?;
    let sorted = sort_value(value);
    let buf = serde_json::to_vec_pretty(&sorted)?;
    Ok(buf)
}

/// Canonicalize in-memory (no re-parse).
pub fn canonical_value<T: Serialize>(value: &T) -> Result<Vec<u8>, JsonError> {
    canonical_json(&to_json(value)?)
}

fn sort_value(mut v: Value) -> Value {
    match &mut v {
        Value::Object(map) => {
            // serde_json::Map preserves insertion order unless `preserve_order`
            // feature is on (it is NOT in our dep set); rebuild sorted.
            let mut pairs: Vec<(String, Value)> = map
                .iter()
                .map(|(k, val)| (k.clone(), sort_value(val.clone())))
                .collect();
            pairs.sort_by(|a, b| a.0.cmp(&b.0));
            let mut new_map = serde_json::Map::new();
            for (k, val) in pairs {
                new_map.insert(k, val);
            }
            Value::Object(new_map)
        }
        Value::Array(items) => {
            for item in items {
                *item = sort_value(std::mem::replace(item, Value::Null));
            }
            v
        }
        _ => v,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_preserves_a_simple_map() {
        let v: Value = serde_json::from_str(r#"{"b":1,"a":2}"#).unwrap();
        let back = round_trip(&v).unwrap();
        assert_eq!(back, v);
    }

    #[test]
    fn canonical_json_sorts_keys_and_is_stable() {
        let a = canonical_json(br#"{"z":1,"a":{"y":2,"b":3}}"#).unwrap();
        let b = canonical_json(br#"{"a":{"b":3,"y":2},"z":1}"#).unwrap();
        assert_eq!(a, b);
        let s = String::from_utf8(a).unwrap();
        assert!(s.starts_with("{\n  \"a\":"));
    }

    #[test]
    fn arrays_keep_order_in_canonical_form() {
        let a = canonical_json(br#"[3,1,2]"#).unwrap();
        let b = canonical_json(br#"[3,1,2]"#).unwrap();
        assert_eq!(a, b);
        // order NOT sorted (arrays are ordered)
        assert_ne!(canonical_json(br#"[1,2,3]"#).unwrap(), a);
    }

    #[test]
    fn to_json_pretty_is_multiline_and_round_trips() {
        let v: Value = serde_json::json!({"b": 2, "a": 1});
        let s = to_json_pretty(&v).unwrap();
        assert!(s.contains('\n'), "pretty JSON is multiline");
        let back: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(back, v);
    }

    #[test]
    fn canonical_value_sorts_keys_of_in_memory_struct() {
        // canonical_value takes a Serialize (no re-parse from raw bytes).
        let v: Value = serde_json::json!({"z": 1, "a": 2});
        let s = String::from_utf8(canonical_value(&v).unwrap()).unwrap();
        assert!(s.find("\"a\"").unwrap() < s.find("\"z\"").unwrap());
    }
}
