//! Field-tree (fieldsV1) path operations — internal to [`crate::apply`].
//!
//! Pure functions over the fieldsV1 JSON tree encoding used by k8s
//! `metadata.managedFields[].fieldsV1`. No dependency on the wire types.

use serde_json::{Map, Value};

use crate::patch::PatchStrategy;

// ---------------------------------------------------------------------------
// Path representation
// ---------------------------------------------------------------------------

/// One segment of a field-ownership path.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) enum Seg {
    /// An object field, e.g. `spec`, `image`.
    Field(String),
    /// A keyed-list element selector — the raw key JSON, e.g. `{"name":"c1"}`.
    Key(String),
}

pub(super) type FieldPath = Vec<Seg>;

/// Render a path as a human-readable dot-joined string.
pub(super) fn format_path(path: &[Seg]) -> String {
    path.iter()
        .map(|s| match s {
            Seg::Field(f) => f.as_str(),
            Seg::Key(k) => k.as_str(),
        })
        .collect::<Vec<_>>()
        .join(".")
}

// ---------------------------------------------------------------------------
// Tree extraction (desired object -> fieldsV1 tree)
// ---------------------------------------------------------------------------

/// Build a fieldsV1 ownership tree from a desired object.
pub fn extract_field_tree(desired: &Value, strategy: &PatchStrategy) -> Value {
    extract_tree_inner(desired, strategy, &mut Vec::new())
}

fn extract_tree_inner(val: &Value, strategy: &PatchStrategy, path: &mut Vec<String>) -> Value {
    let mut tree = Map::new();
    if let Some(obj) = val.as_object() {
        for (key, child) in obj {
            // Skip identity / system fields that are not owned by any
            // field manager: type metadata (apiVersion, kind) and
            // server-managed metadata (name, namespace, managedFields,
            // creationTimestamp, uid, resourceVersion, generation).
            if is_unowned_field(path, key) {
                continue;
            }
            path.push(key.clone());
            let node = extract_node(child, strategy, path);
            path.pop();
            // Skip objects whose entire subtree was filtered (e.g. metadata
            // with only system fields).  Scalars always produce a non-empty
            // ownership marker.
            if child.is_object()
                && node.as_object().map(|o| o.is_empty()).unwrap_or(false)
            {
                continue;
            }
            tree.insert(format!("f:{key}"), node);
        }
    }
    Value::Object(tree)
}

/// Returns true for fields that should never appear in a managedFields
/// ownership tree: type metadata at root level and server-managed
/// metadata fields under `metadata`.
fn is_unowned_field(path: &[String], key: &str) -> bool {
    if path.is_empty() {
        // Root-level type metadata — reconstructed from GVK, not stored.
        return matches!(key, "apiVersion" | "kind");
    }
    if path.len() == 1 && path[0] == "metadata" {
        return matches!(
            key,
            "name" | "namespace" | "managedFields"
                | "creationTimestamp" | "uid" | "resourceVersion"
                | "generation" | "selfLink"
        );
    }
    false
}

fn extract_node(val: &Value, strategy: &PatchStrategy, path: &mut Vec<String>) -> Value {
    match val {
        Value::Object(_) => extract_tree_inner(val, strategy, path),
        Value::Array(arr) => match strategy.merge_key_for(path) {
            Some(mk) => {
                let mut tree = Map::new();
                for el in arr {
                    if let Some(kv) = el.get(mk) {
                        let label = key_object_label(mk, kv);
                        let mut child = extract_tree_inner(el, strategy, path);
                        if let Some(co) = child.as_object_mut() {
                            co.insert(".".to_string(), Value::Object(Map::new()));
                        }
                        tree.insert(label, child);
                    }
                }
                Value::Object(tree)
            }
            None => Value::Object(Map::new()),
        },
        _ => Value::Object(Map::new()),
    }
}

/// Serialise a merge-key field/value pair as the `k:` tree-key JSON.
fn key_object_label(merge_key: &str, key_val: &Value) -> String {
    let mut m = Map::new();
    m.insert(merge_key.to_string(), key_val.clone());
    format!(
        "k:{}",
        serde_json::to_string(&Value::Object(m)).unwrap_or_else(|_| "{}".to_string())
    )
}

// ---------------------------------------------------------------------------
// Tree flattening (fieldsV1 tree -> owned paths)
// ---------------------------------------------------------------------------

/// Flatten a fieldsV1 tree into a list of owned field paths.
pub(super) fn flatten_field_tree(tree: &Value) -> Vec<FieldPath> {
    let mut paths = Vec::new();
    flatten_inner(tree, &mut Vec::new(), &mut paths);
    paths
}

fn flatten_inner(tree: &Value, current: &mut Vec<Seg>, paths: &mut Vec<FieldPath>) {
    if let Some(obj) = tree.as_object() {
        for (key, child) in obj {
            if key == "." {
                continue;
            }
            if let Some(f) = key.strip_prefix("f:") {
                current.push(Seg::Field(f.to_string()));
                // Only register leaf paths for f: nodes (scalars and atomic
                // arrays).  Intermediate containers like `data` are not
                // conflict points — two managers can own different children.
                let has_children = child
                    .as_object()
                    .map(|o| o.keys().any(|k| k.starts_with("f:") || k.starts_with("k:")))
                    .unwrap_or(false);
                if !has_children {
                    paths.push(current.clone());
                }
                flatten_inner(child, current, paths);
                current.pop();
            } else if let Some(k) = key.strip_prefix("k:") {
                current.push(Seg::Key(k.to_string()));
                // Keyed-list elements are always owned as units.
                paths.push(current.clone());
                flatten_inner(child, current, paths);
                current.pop();
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Path-based field removal (pruning)
// ---------------------------------------------------------------------------

/// Remove a field at the given path from `value`.
pub(super) fn remove_path(value: &mut Value, path: &[Seg]) {
    if path.is_empty() {
        return;
    }
    match &path[0] {
        Seg::Field(name) => {
            if let Some(obj) = value.as_object_mut() {
                if path.len() == 1 {
                    obj.remove(name);
                } else if let Some(child) = obj.get_mut(name) {
                    remove_path(child, &path[1..]);
                }
            }
        }
        Seg::Key(key_json) => {
            if let Some(arr) = value.as_array_mut() {
                if let Some(idx) = find_by_key(arr, key_json) {
                    if path.len() == 1 {
                        arr.remove(idx);
                    } else {
                        remove_path(&mut arr[idx], &path[1..]);
                    }
                }
            }
        }
    }
}

/// Find the array element matching a `{"key":"val"}` JSON string.
fn find_by_key(arr: &[Value], key_json: &str) -> Option<usize> {
    let parsed: Value = serde_json::from_str(key_json).ok()?;
    let obj = parsed.as_object()?;
    let (field, val) = obj.iter().next()?;
    arr.iter()
        .position(|el| el.get(field).map(|v| v == val).unwrap_or(false))
}

// ---------------------------------------------------------------------------
// Tree rebuilding (paths -> fieldsV1 tree)
// ---------------------------------------------------------------------------

/// Rebuild a fieldsV1 tree from a set of owned paths.
pub(super) fn build_tree_from_paths(paths: &[FieldPath]) -> Value {
    let mut root = Map::new();
    for p in paths {
        insert_path(&mut root, p);
    }
    Value::Object(root)
}

fn insert_path(tree: &mut Map<String, Value>, path: &[Seg]) {
    if path.is_empty() {
        return;
    }
    let key = match &path[0] {
        Seg::Field(f) => format!("f:{f}"),
        Seg::Key(k) => format!("k:{k}"),
    };
    let child = tree.entry(key).or_insert_with(|| Value::Object(Map::new()));
    if let Some(co) = child.as_object_mut() {
        if path.len() == 1 && matches!(path[0], Seg::Key(_)) {
            co.entry(".".to_string()).or_insert(Value::Object(Map::new()));
        } else if path.len() > 1 {
            insert_path(co, &path[1..]);
        }
    }
}
