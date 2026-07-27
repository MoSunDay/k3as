//! Strategic Merge Patch (SMP) + JSON Patch fallback (TODO **T1.1**, step S5).
//!
//! Kubernetes server-side apply uses Strategic Merge Patch semantics:
//!
//! - **maps**: merged recursively (key-by-key).
//! - **scalars**: patch value replaces the target value.
//! - **`null`** in the patch: deletes the field from the target.
//! - **lists with a merge key** (e.g. `containers` by `name`): elements are
//!   matched on the merge key and merged in place; new elements are appended
//!   (order preserved, matching kubectl behavior).
//! - **lists without a merge key**: replaced atomically (default k8s behavior).
//! - **`{"$patch": "delete"}`**: deletes a map field or removes a list element.
//!
//! This implements the *core* merge semantics (containers, volumes, ports, env,
//! labels/annotations) — the documented v1 scope. Full SMP fidelity (every
//! patchStrategy/mergeKey declared in OpenAPI) is a later concern; the
//! [`PatchStrategy`] map is the extension seam.
//!
//! JSON Patch (RFC 6902) is provided as a fallback for `Content-Type:
//! application/json-patch+json` via the `json-patch` crate.

use std::collections::HashMap;

use serde_json::Value;
use thiserror::Error;

pub use json_patch::Patch as JsonPatch;

/// SMP error.
#[derive(Debug, Error)]
pub enum PatchError {
    #[error("json patch application failed: {0}")]
    JsonPatch(#[from] json_patch::PatchError),
    #[error("invalid patch document: {0}")]
    Invalid(String),
}

/// Declares which object paths are "merge by key" lists and their merge key.
///
/// Default = the core/v1 patch strategies (containers/init/ephemeral containers,
/// volumes, ports, env). Unknown lists fall back to atomic replace.
#[derive(Debug, Clone, Default)]
pub struct PatchStrategy {
    /// Keyed by slash-joined JSON path (e.g. `"spec/containers"`), value = the
    /// merge key field name (e.g. `"name"`).
    merge_keys: HashMap<String, String>,
}

impl PatchStrategy {
    /// The Kubernetes core/v1 patch strategies (the documented v1 scope).
    pub fn kubernetes_defaults() -> Self {
        let mut s = Self::default();
        // containers, initContainers, ephemeralContainers -> merge by name
        for field in ["containers", "initContainers", "ephemeralContainers"] {
            s.merge_keys
                .insert(format!("spec/{field}"), "name".to_string());
        }
        // volumes -> merge by name
        s.merge_keys
            .insert("spec/volumes".to_string(), "name".to_string());
        // container ports -> merge by containerPort
        s.merge_keys.insert(
            "spec/containers/ports".to_string(),
            "containerPort".to_string(),
        );
        // env vars -> merge by name
        s.merge_keys
            .insert("spec/containers/env".to_string(), "name".to_string());
        // imagePullSecrets / local volumes -> by name
        s.merge_keys
            .insert("spec/imagePullSecrets".to_string(), "name".to_string());
        s
    }

    /// Register an additional merge-by-key list at a slash path.
    pub fn with_merge_key(mut self, path: &str, key: &str) -> Self {
        self.merge_keys.insert(path.to_string(), key.to_string());
        self
    }

    fn merge_key_for(&self, path: &[String]) -> Option<&str> {
        let joined = path.join("/");
        self.merge_keys.get(&joined).map(String::as_str)
    }
}

/// Apply a Strategic Merge Patch to `target` in place.
pub fn strategic_merge(
    target: &mut Value,
    patch: &Value,
    strategy: &PatchStrategy,
) -> Result<(), PatchError> {
    merge_value(target, patch, &mut Vec::new(), strategy)
}

fn merge_value(
    target: &mut Value,
    patch: &Value,
    path: &mut Vec<String>,
    strategy: &PatchStrategy,
) -> Result<(), PatchError> {
    match patch {
        Value::Object(patch_map) => {
            // $patch: delete at a map level deletes the whole field — handled by
            // the caller when iterating the parent. At the top level it's a no-op
            // marker we ignore for the object itself.
            if matches!(patch_map.get("$patch"), Some(Value::String(s)) if s == "delete") {
                // Signal deletion by nulling; the parent loop removes null'd keys.
                *target = Value::Null;
                return Ok(());
            }
            let target_map = ensure_object(target);
            let keys: Vec<String> = patch_map.keys().cloned().collect();
            for key in keys {
                let pv = patch_map.get(&key).cloned().unwrap_or(Value::Null);
                path.push(key.clone());
                if pv.is_null() {
                    // null => delete the field
                    target_map.remove(&key);
                } else if is_delete_directive(&pv) {
                    target_map.remove(&key);
                } else {
                    let entry = target_map.entry(key.clone()).or_insert(Value::Null);
                    if entry.is_object() && pv.is_object() {
                        merge_value(entry, &pv, path, strategy)?;
                    } else if entry.is_array() && pv.is_array() {
                        merge_list(entry, &pv, path, strategy)?;
                    } else {
                        // scalar / type-change => replace
                        *entry = pv;
                    }
                }
                path.pop();
            }
            Ok(())
        }
        // A top-level (or field-level) scalar/Array patch replaces directly; the
        // map recursion above already routes arrays to merge_list.
        other => {
            *target = other.clone();
            Ok(())
        }
    }
}

/// Merge two arrays. If the current path is a registered merge-key list, merge
/// elements by key (preserving target order, appending new ones); otherwise
/// replace atomically.
fn merge_list(
    target: &mut Value,
    patch: &Value,
    path: &[String],
    strategy: &PatchStrategy,
) -> Result<(), PatchError> {
    let Some(merge_key) = strategy.merge_key_for(path) else {
        // atomic replace (default k8s behavior for non-keyed lists)
        *target = patch.clone();
        return Ok(());
    };
    let target_arr = target
        .as_array_mut()
        .ok_or_else(|| PatchError::Invalid("merge-key list target is not an array".to_string()))?;
    let patch_arr = patch.as_array().expect("checked by caller");

    let mut result: Vec<Value> = Vec::with_capacity(target_arr.len() + patch_arr.len());
    // index existing elements by their merge-key value for O(1) lookup
    let mut existing: HashMap<String, usize> = HashMap::new();
    let mut deleted: Vec<bool> = Vec::new();

    // Carry over all target elements (order preserved), recording their key.
    for el in target_arr.drain(..) {
        if let Some(k) = key_of(&el, merge_key) {
            existing.insert(k, result.len());
        }
        deleted.push(false);
        result.push(el);
    }

    // Fold in patch elements: match-and-merge, delete, or append.
    let mut sub_path = path.to_vec();
    for pel in patch_arr {
        let pkey = key_of(pel, merge_key);
        let matched = pkey.as_ref().and_then(|k| existing.get(k).copied());

        if is_delete_directive(pel) {
            if let Some(idx) = matched {
                deleted[idx] = true; // tombstone
            }
            continue;
        }
        match (pkey, matched) {
            (Some(_), Some(idx)) => {
                sub_path.clear();
                sub_path.extend_from_slice(path);
                merge_value(&mut result[idx], pel, &mut sub_path, strategy)?;
            }
            (Some(k), None) => {
                existing.insert(k, result.len());
                deleted.push(false);
                result.push(pel.clone());
            }
            (None, _) => {
                // patch element without a merge key: append verbatim
                deleted.push(false);
                result.push(pel.clone());
            }
        }
    }
    // drop tombstoned elements, preserving order of the survivors
    let survivors: Vec<Value> = result
        .into_iter()
        .enumerate()
        .filter_map(|(i, v)| {
            if deleted.get(i).copied().unwrap_or(false) {
                None
            } else {
                Some(v)
            }
        })
        .collect();
    *target = Value::Array(survivors);
    Ok(())
}

fn key_of(el: &Value, merge_key: &str) -> Option<String> {
    let v = el.get(merge_key)?;
    // merge keys are either strings ("name") or numbers ("containerPort")
    if let Some(s) = v.as_str() {
        Some(s.to_string())
    } else {
        v.as_i64().map(|n| n.to_string())
    }
}

fn is_delete_directive(v: &Value) -> bool {
    matches!(v.get("$patch"), Some(Value::String(s)) if s == "delete")
}

fn ensure_object(v: &mut Value) -> &mut serde_json::Map<String, Value> {
    if !v.is_object() {
        *v = Value::Object(serde_json::Map::new());
    }
    v.as_object_mut().expect("just ensured object")
}

/// Apply an RFC 6902 JSON Patch in place (fallback codec for
/// `application/json-patch+json`).
pub fn apply_json_patch(target: &mut Value, patch: &JsonPatch) -> Result<(), PatchError> {
    json_patch::patch(target, patch).map_err(PatchError::JsonPatch)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn merge(target: Value, patch: Value) -> Value {
        let mut t = target;
        strategic_merge(&mut t, &patch, &PatchStrategy::kubernetes_defaults()).unwrap();
        t
    }

    #[test]
    fn scalar_replace() {
        assert_eq!(merge(json!({"a": 1}), json!({"a": 2})), json!({"a": 2}));
    }

    #[test]
    fn null_deletes_field() {
        assert_eq!(
            merge(json!({"a": 1, "b": 2}), json!({"a": null})),
            json!({"b": 2})
        );
    }

    #[test]
    fn map_recursive_merge() {
        assert_eq!(
            merge(json!({"spec": {"a": 1, "b": 2}}), json!({"spec": {"b": 3}})),
            json!({"spec": {"a": 1, "b": 3}})
        );
    }

    #[test]
    fn containers_merge_by_name_in_place() {
        let target = json!({
            "spec": {"containers": [
                {"name": "a", "image": "img-a:1"},
                {"name": "b", "image": "img-b:1"}
            ]}
        });
        let patch = json!({
            "spec": {"containers": [{"name": "a", "image": "img-a:2"}]}
        });
        let out = merge(target, patch);
        assert_eq!(
            out["spec"]["containers"],
            json!([
                {"name": "a", "image": "img-a:2"},
                {"name": "b", "image": "img-b:1"}
            ])
        );
    }

    #[test]
    fn new_container_is_appended_preserving_order() {
        let target = json!({"spec": {"containers": [{"name": "a"}]}});
        let patch = json!({"spec": {"containers": [{"name": "b"}, {"name": "a", "image": "x"}]}});
        let out = merge(target, patch);
        let names: Vec<&str> = out["spec"]["containers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["a", "b"]);
    }

    #[test]
    fn patch_delete_directive_removes_container() {
        let target = json!({"spec": {"containers": [{"name": "a"}, {"name": "b"}]}});
        let patch = json!({"spec": {"containers": [{"name": "a", "$patch": "delete"}]}});
        let out = merge(target, patch);
        let names: Vec<&str> = out["spec"]["containers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["b"]);
    }

    #[test]
    fn volumes_merge_by_name() {
        let target = json!({"spec": {"volumes": [{"name": "v1", "emptyDir": {}}]}});
        let patch =
            json!({"spec": {"volumes": [{"name": "v1", "emptyDir": {"medium": "Memory"}}]}});
        let out = merge(target, patch);
        assert_eq!(
            out["spec"]["volumes"][0]["emptyDir"]["medium"],
            json!("Memory")
        );
    }

    #[test]
    fn non_keyed_list_is_replaced_atomically() {
        let target = json!({"spec": {"args": ["a", "b"]}});
        let patch = json!({"spec": {"args": ["x"]}});
        let out = merge(target, patch);
        // args has no merge key -> atomic replace
        assert_eq!(out["spec"]["args"], json!(["x"]));
    }

    #[test]
    fn labels_and_annotations_deep_merge() {
        let target = json!({"metadata": {"labels": {"a": "1", "b": "2"}}});
        let patch = json!({"metadata": {"labels": {"b": "3", "c": "4"}}});
        let out = merge(target, patch);
        assert_eq!(
            out["metadata"]["labels"],
            json!({"a": "1", "b": "3", "c": "4"})
        );
    }

    #[test]
    fn json_patch_fallback_applies() {
        let mut target = json!({"a": "b"});
        let patch: JsonPatch =
            serde_json::from_str(r#"[{"op":"add","path":"/c","value":"d"}]"#).unwrap();
        apply_json_patch(&mut target, &patch).unwrap();
        assert_eq!(target, json!({"a": "b", "c": "d"}));
    }

    #[test]
    fn with_merge_key_adds_a_custom_merge_strategy() {
        // Registering a custom merge key for an otherwise-unknown list path
        // makes it merge in place by that key instead of atomic replace.
        let strategy = PatchStrategy::default().with_merge_key("spec/networks", "name");
        let target = json!({"spec": {"networks": [{"name": "n1", "value": "a", "extra": "x"}]}});
        let patch = json!({"spec": {"networks": [{"name": "n1", "value": "b"}]}});
        let mut out = target;
        strategic_merge(&mut out, &patch, &strategy).unwrap();
        // merged by name: value updated, extra preserved
        assert_eq!(out["spec"]["networks"][0]["value"], json!("b"));
        assert_eq!(out["spec"]["networks"][0]["extra"], json!("x"));

        // Without the strategy the same list is atomically replaced (extra lost).
        let mut out2 = json!({"spec": {"networks": [{"name": "n1", "value": "a", "extra": "x"}]}});
        strategic_merge(&mut out2, &patch, &PatchStrategy::default()).unwrap();
        assert!(out2["spec"]["networks"][0].get("extra").is_none());
    }
}
