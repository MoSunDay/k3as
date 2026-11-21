//! `rbac.authorization.k8s.io/v1` — RBAC supported-subset wire types
//! (TODO **T1.3b**, decisions **Q47/Q59**).
//!
//! Hand-written in-house wire types (the established pattern for this
//! repo's supported-subset posture, Q59 — cf. `argoproj.rs`/`networking`),
//! NOT k8s-openapi `register_native`: registration stays conditional on
//! `--token-file` (T1.3a default-off red line: the h2c/no-auth posture
//! serves a byte-identical discovery document, golden G02/G03).
//!
//! Supported subset: the four objects `Role`/`RoleBinding`/`ClusterRole`/
//! `ClusterRoleBinding` with `PolicyRule`/`Subject`/`RoleRef` semantics —
//! enough for the apiserver authz middleware (`apiserver::authz`) to do
//! verbs × apiGroups × resources × resourceNames matching. No
//! `aggregationRule`, no escalation/overlap tooling, no subresources in
//! rules (the base resource matches). Wire format is JSON-only (Q10).

use kube_core::metadata::{ListMeta, ObjectMeta, TypeMeta};
use serde::{Deserialize, Serialize};

use crate::schema::{SchemaRegistry, Scope};

/// API group + version.
pub const GROUP: &str = "rbac.authorization.k8s.io";
pub const VERSION: &str = "v1";
pub const API_VERSION: &str = "rbac.authorization.k8s.io/v1";

// ============================ Role (namespaced) ============================
/// `Role` — namespaced permission set.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Role {
    #[serde(flatten)]
    pub types: TypeMeta,
    #[serde(default)]
    pub metadata: ObjectMeta,
    #[serde(default)]
    pub rules: Vec<PolicyRule>,
}

/// `RoleList`.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct RoleList {
    #[serde(flatten)]
    pub types: TypeMeta,
    #[serde(default)]
    pub metadata: ListMeta,
    #[serde(default)]
    pub items: Vec<Role>,
}

// =========================== ClusterRole (cluster) =========================

/// `ClusterRole` — cluster-scoped permission set.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct ClusterRole {
    #[serde(flatten)]
    pub types: TypeMeta,
    #[serde(default)]
    pub metadata: ObjectMeta,
    #[serde(default)]
    pub rules: Vec<PolicyRule>,
}

/// `ClusterRoleList`.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct ClusterRoleList {
    #[serde(flatten)]
    pub types: TypeMeta,
    #[serde(default)]
    pub metadata: ListMeta,
    #[serde(default)]
    pub items: Vec<ClusterRole>,
}

// ============================== PolicyRule =================================

/// One permission rule: any of `verbs` × `api_groups` × `resources`
/// (`resource_names` empty = every object of the resource).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PolicyRule {
    /// Verbs; `"*"` = all. Upstream camelCase wire field `apiGroups`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub verbs: Vec<String>,
    /// API groups; `""` = core, `"*"` = all.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub api_groups: Vec<String>,
    /// Resources (plural); `"*"` = all. Subresources are not matched
    /// separately in this subset (the base resource covers them).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub resources: Vec<String>,
    /// Restrict to named objects; empty = all names.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub resource_names: Vec<String>,
    /// Non-resource URLs (`/healthz`, ...): accepted on the wire; the
    /// subset treats every non-resource endpoint as authenticated-allow.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub non_resource_urls: Vec<String>,
}

impl PolicyRule {
    /// Does this rule grant `verb` on `group`/`resource`/`name`?
    /// Pure matcher shared by unit tests and the authz middleware.
    pub fn grants(&self, verb: &str, group: &str, resource: &str, name: Option<&str>) -> bool {
        let verb_ok = self.verbs.iter().any(|v| v == "*" || v == verb);
        let group_ok = self.api_groups.iter().any(|g| g == "*" || g == group);
        let resource_ok = self.resources.iter().any(|r| r == "*" || r == resource);
        // Upstream semantics: `resourceNames` restricts to the named
        // objects only and NEVER grants nameless (list/watch) requests.
        let name_ok = match name {
            None => self.resource_names.is_empty(),
            Some(n) => {
                self.resource_names.is_empty() || self.resource_names.iter().any(|rn| rn == n)
            }
        };
        verb_ok && group_ok && resource_ok && name_ok
    }
}

// ============================== RoleBinding ================================

/// `RoleBinding` — grants a Role (in the binding's namespace) to subjects.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RoleBinding {
    #[serde(flatten)]
    pub types: TypeMeta,
    #[serde(default)]
    pub metadata: ObjectMeta,
    #[serde(default)]
    pub role_ref: RoleRef,
    #[serde(default)]
    pub subjects: Vec<Subject>,
}

/// `RoleBindingList`.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct RoleBindingList {
    #[serde(flatten)]
    pub types: TypeMeta,
    #[serde(default)]
    pub metadata: ListMeta,
    #[serde(default)]
    pub items: Vec<RoleBinding>,
}

// ============================ ClusterRoleBinding ===========================

/// `ClusterRoleBinding` — grants a ClusterRole cluster-wide.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ClusterRoleBinding {
    #[serde(flatten)]
    pub types: TypeMeta,
    #[serde(default)]
    pub metadata: ObjectMeta,
    #[serde(default)]
    pub role_ref: RoleRef,
    #[serde(default)]
    pub subjects: Vec<Subject>,
}

/// `ClusterRoleBindingList`.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct ClusterRoleBindingList {
    #[serde(flatten)]
    pub types: TypeMeta,
    #[serde(default)]
    pub metadata: ListMeta,
    #[serde(default)]
    pub items: Vec<ClusterRoleBinding>,
}

/// Reference from a binding to its Role/ClusterRole.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RoleRef {
    pub api_group: String,
    pub kind: String,
    pub name: String,
}

/// Who a binding grants: `User`, `Group`, or `ServiceAccount`
/// (T1.3b minimal ServiceAccount face, D7 — no SA token controller, T3.3).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Subject {
    pub kind: String,
    pub api_group: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub namespace: String,
}

impl Subject {
    /// Does this subject match `user`/`groups` (from token-file authn)?
    /// `ServiceAccount` subjects match the conventional group
    /// `system:serviceaccount:<ns>:<name>` (D7 subset: static tokens carry
    /// SA identity as a group; no SA token controller exists yet).
    pub fn matches(&self, user: &str, groups: &[String]) -> bool {
        match self.kind.as_str() {
            "User" => self.name == user,
            "Group" => groups.iter().any(|g| g == &self.name),
            "ServiceAccount" => {
                let convention = format!("system:serviceaccount:{}:{}", self.namespace, self.name);
                groups.contains(&convention)
            }
            _ => false,
        }
    }
}

// ============================== registration ===============================

/// Register the four RBAC kinds into a [`SchemaRegistry`]. Called by the
/// CLI discovery wiring ONLY when `--token-file` arms authentication —
/// the default (no-auth) schema stays byte-identical (golden G02/G03).
pub fn register(registry: &mut SchemaRegistry) {
    registry.register(
        GROUP,
        VERSION,
        "Role",
        "roles",
        "RoleList",
        Scope::Namespaced,
    );
    registry.register(
        GROUP,
        VERSION,
        "ClusterRole",
        "clusterroles",
        "ClusterRoleList",
        Scope::Cluster,
    );
    registry.register(
        GROUP,
        VERSION,
        "RoleBinding",
        "rolebindings",
        "RoleBindingList",
        Scope::Namespaced,
    );
    registry.register(
        GROUP,
        VERSION,
        "ClusterRoleBinding",
        "clusterrolebindings",
        "ClusterRoleBindingList",
        Scope::Cluster,
    );
}

/// Constructors with correct `TypeMeta` (bootstrap + tests).
pub fn new_cluster_role(name: &str, rules: Vec<PolicyRule>) -> ClusterRole {
    ClusterRole {
        types: TypeMeta {
            api_version: API_VERSION.to_string(),
            kind: "ClusterRole".to_string(),
        },
        metadata: ObjectMeta {
            name: Some(name.to_string()),
            ..ObjectMeta::default()
        },
        rules,
    }
}

/// `ClusterRoleBinding` from one Group subject to a ClusterRole.
pub fn new_group_cluster_role_binding(name: &str, group: &str, role: &str) -> ClusterRoleBinding {
    ClusterRoleBinding {
        types: TypeMeta {
            api_version: API_VERSION.to_string(),
            kind: "ClusterRoleBinding".to_string(),
        },
        metadata: ObjectMeta {
            name: Some(name.to_string()),
            ..ObjectMeta::default()
        },
        role_ref: RoleRef {
            api_group: GROUP.to_string(),
            kind: "ClusterRole".to_string(),
            name: role.to_string(),
        },
        subjects: vec![Subject {
            kind: "Group".to_string(),
            api_group: GROUP.to_string(),
            name: group.to_string(),
            namespace: String::new(),
        }],
    }
}

#[cfg(test)]
mod tests;
