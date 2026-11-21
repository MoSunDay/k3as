//! Unit tests for the rbac wire types (TODO **T1.3b**, Q47/Q59).

use super::*;

#[test]
fn policy_rule_grants_matrix() {
    let rule = PolicyRule {
        verbs: vec!["get".into(), "list".into()],
        api_groups: vec!["".into(), "apps".into()],
        resources: vec!["pods".into()],
        resource_names: vec!["only-this".into()],
        non_resource_urls: Vec::new(),
    };
    assert!(rule.grants("get", "", "pods", Some("only-this")));
    assert!(!rule.grants("get", "", "pods", Some("other")));
    assert!(!rule.grants("get", "", "pods", None), "list-all denied");
    assert!(!rule.grants("delete", "", "pods", Some("only-this")));
    assert!(!rule.grants("get", "coordination.k8s.io", "pods", Some("only-this")));
}

#[test]
fn policy_rule_wildcards() {
    let all = PolicyRule {
        verbs: vec!["*".into()],
        api_groups: vec!["*".into()],
        resources: vec!["*".into()],
        resource_names: Vec::new(),
        non_resource_urls: Vec::new(),
    };
    assert!(all.grants("delete", "apps", "deployments", Some("x")));
}

#[test]
fn subject_matching_kinds() {
    let u = Subject {
        kind: "User".into(),
        name: "alice".into(),
        ..Subject::default()
    };
    assert!(u.matches("alice", &[]));
    assert!(!u.matches("bob", &[]));
    let g = Subject {
        kind: "Group".into(),
        name: "system:masters".into(),
        ..Subject::default()
    };
    assert!(g.matches("x", &["system:masters".into()]));
    let sa = Subject {
        kind: "ServiceAccount".into(),
        namespace: "default".into(),
        name: "deploy".into(),
        ..Subject::default()
    };
    assert!(sa.matches("any", &["system:serviceaccount:default:deploy".to_string()]));
    assert!(!sa.matches("any", &[]));
}

#[test]
fn register_covers_four_kinds() {
    use kube_core::gvk::GroupVersionKind;
    let mut reg = SchemaRegistry::new();
    register(&mut reg);
    for (kind, plural, namespaced) in [
        ("Role", "roles", true),
        ("ClusterRole", "clusterroles", false),
        ("RoleBinding", "rolebindings", true),
        ("ClusterRoleBinding", "clusterrolebindings", false),
    ] {
        let info = reg
            .get(&GroupVersionKind::gvk(GROUP, VERSION, kind))
            .unwrap_or_else(|| panic!("{kind} registered"));
        assert_eq!(info.resource, plural);
        assert_eq!(info.scope.is_namespaced(), namespaced, "{kind} scope");
    }
}

#[test]
fn constructors_carry_typemeta() {
    let cr = new_cluster_role(
        "cluster-admin",
        vec![PolicyRule {
            verbs: vec!["*".into()],
            api_groups: vec!["*".into()],
            resources: vec!["*".into()],
            ..PolicyRule::default()
        }],
    );
    assert_eq!(cr.types.api_version, API_VERSION);
    assert_eq!(cr.types.kind, "ClusterRole");
    let crb = new_group_cluster_role_binding("b", "system:masters", "cluster-admin");
    assert_eq!(crb.role_ref.kind, "ClusterRole");
    assert_eq!(crb.subjects[0].kind, "Group");
}

#[test]
fn bindings_wire_camelform_pinned() {
    // Upstream wire format: roleRef / apiGroups / resourceNames —
    // the authz middleware decodes raw stored JSON, so a snake_case
    // leak here would silently drop every RoleBinding grant (S50).
    let rb = RoleBinding {
        role_ref: RoleRef {
            api_group: GROUP.into(),
            kind: "Role".into(),
            name: "r".into(),
        },
        ..RoleBinding::default()
    };
    let v = serde_json::to_value(&rb).unwrap();
    assert!(v.get("roleRef").is_some(), "roleRef camelCase on the wire");
    assert!(v.get("role_ref").is_none(), "snake_case must not leak");
    let back: RoleBinding = serde_json::from_value(serde_json::json!({
        "apiVersion": API_VERSION,
        "kind": "RoleBinding",
        "metadata": {"name": "b", "namespace": "default"},
        "roleRef": {"apiGroup": GROUP, "kind": "Role", "name": "r"},
        "subjects": [{"kind": "User", "name": "alice", "apiGroup": GROUP}],
    }))
    .unwrap();
    assert_eq!(back.role_ref.name, "r");
    assert_eq!(back.subjects[0].name, "alice");
}
