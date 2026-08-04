//! Server-Side Apply (SSA) field-manager logic — TODO **T1.2c**.
//!
//! Pure-functional SSA algorithm with no HTTP dependency. Implements the core
//! k8s server-side apply semantics: field ownership tracking via
//! `metadata.managedFields[]`, conflict detection, and force override.
//!
//! **Scope A**: object fields + keyed-list ownership by merge key
//! (e.g. `containers` by `name`). Atomic lists are owned as a unit. Full
//! fieldsV1 edge cases (`i:`/`v:` indexes, atom replacement) are deferred.
//!
//! Field ownership is an apiserver-layer concern — `managedFields` lives
//! inside `value.metadata.managedFields[]` (no storage-layer changes).

mod field_set;

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::patch::{strategic_merge, PatchStrategy};
use field_set::{remove_path, format_path, build_tree_from_paths, flatten_field_tree, FieldPath};

// ---------------------------------------------------------------------------
// Wire model (matches k8s `metadata.managedFields[]`)
// ---------------------------------------------------------------------------

/// One entry in `metadata.managedFields[]`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ManagedFieldEntry {
    pub manager: String,
    pub operation: Operation,
    #[serde(rename = "apiVersion")]
    pub api_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time: Option<String>,
    #[serde(rename = "fieldsType")]
    pub fields_type: String,
    #[serde(rename = "fieldsV1")]
    pub fields_v1: Value,
}

/// The operation that set the fields.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum Operation {
    Apply,
    Update,
}

/// A fieldsV1 ownership tree wrapper.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FieldTree(pub Value);

// ---------------------------------------------------------------------------
// Options / result
// ---------------------------------------------------------------------------

/// Options passed to [`apply_object`].
#[derive(Debug, Clone)]
pub struct ApplyOptions {
    pub field_manager: String,
    pub force: bool,
    pub api_version: String,
    pub time: Option<String>,
}

impl Default for ApplyOptions {
    fn default() -> Self {
        Self {
            field_manager: "init-pro".to_string(),
            force: false,
            api_version: "v1".to_string(),
            time: None,
        }
    }
}

/// A detected ownership conflict.
#[derive(Debug, Clone, PartialEq)]
pub struct Conflict {
    pub path: String,
    pub manager: String,
}

/// The outcome of [`apply_object`].
#[derive(Debug, Clone)]
pub struct ApplyResult {
    pub value: Value,
    pub managed_fields: Vec<ManagedFieldEntry>,
    pub conflicts: Vec<Conflict>,
    pub created: bool,
}

// ---------------------------------------------------------------------------
// managedFields read/write on a Value
// ---------------------------------------------------------------------------

/// Read `metadata.managedFields` from a JSON object.
pub fn get_managed_fields(value: &Value) -> Vec<ManagedFieldEntry> {
    value
        .get("metadata")
        .and_then(|m| m.get("managedFields"))
        .and_then(|mf| mf.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|e| serde_json::from_value(e.clone()).ok())
                .collect()
        })
        .unwrap_or_default()
}

/// Write `metadata.managedFields` on a JSON object (in place).
pub fn set_managed_fields(value: &mut Value, fields: Vec<ManagedFieldEntry>) {
    let arr: Vec<Value> = fields
        .iter()
        .filter_map(|f| serde_json::to_value(f).ok())
        .collect();
    // Ensure metadata exists (the object may not have it yet).
    if value.get("metadata").is_none() {
        if let Some(obj) = value.as_object_mut() {
            obj.insert("metadata".to_string(), Value::Object(serde_json::Map::new()));
        }
    }
    if let Some(m) = value.get_mut("metadata").and_then(|m| m.as_object_mut()) {
        m.insert("managedFields".to_string(), Value::Array(arr));
    }
}

fn strip_managed_fields(value: &mut Value) {
    if let Some(meta) = value.get_mut("metadata").and_then(|m| m.as_object_mut()) {
        meta.remove("managedFields");
    }
}

// ---------------------------------------------------------------------------
// Re-exports from field_set
// ---------------------------------------------------------------------------

pub use field_set::extract_field_tree;

// ---------------------------------------------------------------------------
// Ownership + conflict logic
// ---------------------------------------------------------------------------

fn build_apply_ownership(entries: &[ManagedFieldEntry]) -> HashMap<FieldPath, String> {
    let mut map = HashMap::new();
    for e in entries {
        if e.operation != Operation::Apply {
            continue;
        }
        for p in flatten_field_tree(&e.fields_v1) {
            map.insert(p, e.manager.clone());
        }
    }
    map
}

fn detect_conflicts(
    desired_paths: &[FieldPath],
    ownership: &HashMap<FieldPath, String>,
    mgr: &str,
) -> Vec<Conflict> {
    desired_paths
        .iter()
        .filter_map(|p| {
            ownership.get(p).and_then(|owner| {
                (owner != mgr).then(|| Conflict {
                    path: format_path(p),
                    manager: owner.clone(),
                })
            })
        })
        .collect()
}

fn manager_apply_paths(entries: &[ManagedFieldEntry], mgr: &str) -> Vec<FieldPath> {
    entries
        .iter()
        .filter(|e| e.manager == mgr && e.operation == Operation::Apply)
        .flat_map(|e| flatten_field_tree(&e.fields_v1))
        .collect()
}

fn update_fields(
    mut entries: Vec<ManagedFieldEntry>,
    desired_paths: &[FieldPath],
    desired_tree: &Value,
    opts: &ApplyOptions,
) -> Vec<ManagedFieldEntry> {
    if opts.force {
        let ds: HashSet<&FieldPath> = desired_paths.iter().collect();
        for e in entries.iter_mut() {
            if e.operation != Operation::Apply || e.manager == opts.field_manager {
                continue;
            }
            let kept: Vec<_> = flatten_field_tree(&e.fields_v1)
                .into_iter()
                .filter(|p| !ds.contains(p))
                .collect();
            e.fields_v1 = build_tree_from_paths(&kept);
        }
    }
    let new_entry = ManagedFieldEntry {
        manager: opts.field_manager.clone(),
        operation: Operation::Apply,
        api_version: opts.api_version.clone(),
        time: opts.time.clone(),
        fields_type: "FieldsV1".to_string(),
        fields_v1: desired_tree.clone(),
    };
    if let Some(i) = entries
        .iter()
        .position(|e| e.manager == opts.field_manager && e.operation == Operation::Apply)
    {
        entries[i] = new_entry;
    } else {
        entries.push(new_entry);
    }
    entries.retain(|e| !e.fields_v1.as_object().map(|o| o.is_empty()).unwrap_or(true));
    entries
}

// ---------------------------------------------------------------------------
// Core entry point
// ---------------------------------------------------------------------------

/// Apply `desired` to `live` using server-side apply semantics.
///
/// Returns the merged value, updated managed fields, and any conflicts.
/// When `opts.force` is `true`, conflicts are resolved by transferring
/// ownership instead of returning them.
pub fn apply_object(
    live: Option<&Value>,
    desired: &Value,
    opts: &ApplyOptions,
    strategy: &PatchStrategy,
) -> ApplyResult {
    let desired_tree = extract_field_tree(desired, strategy);
    let desired_paths = flatten_field_tree(&desired_tree);

    // ---- Create path (no live object) ----
    if live.is_none() {
        let mut value = desired.clone();
        strip_managed_fields(&mut value);
        let entry = ManagedFieldEntry {
            manager: opts.field_manager.clone(),
            operation: Operation::Apply,
            api_version: opts.api_version.clone(),
            time: opts.time.clone(),
            fields_type: "FieldsV1".to_string(),
            fields_v1: desired_tree,
        };
        return ApplyResult {
            value,
            managed_fields: vec![entry],
            conflicts: Vec::new(),
            created: true,
        };
    }

    let live_val = live.unwrap();
    let existing = get_managed_fields(live_val);

    // ---- Conflict detection ----
    let apply_ownership = build_apply_ownership(&existing);
    let conflicts = detect_conflicts(&desired_paths, &apply_ownership, &opts.field_manager);

    if !conflicts.is_empty() && !opts.force {
        return ApplyResult {
            value: live_val.clone(),
            managed_fields: existing,
            conflicts,
            created: false,
        };
    }

    // ---- Merge + prune ----
    let mut merged = live_val.clone();
    strip_managed_fields(&mut merged);

    let old_paths = manager_apply_paths(&existing, &opts.field_manager);
    let ds: HashSet<&FieldPath> = desired_paths.iter().collect();
    for old in &old_paths {
        if !ds.contains(old) {
            remove_path(&mut merged, old);
        }
    }

    let _ = strategic_merge(&mut merged, desired, strategy);
    strip_managed_fields(&mut merged);

    let updated = update_fields(existing, &desired_paths, &desired_tree, opts);

    ApplyResult {
        value: merged,
        managed_fields: updated,
        conflicts: Vec::new(),
        created: false,
    }
}
