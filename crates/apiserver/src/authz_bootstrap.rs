//! RBAC bootstrap seeding (TODO **T1.3b**, decision **Q47** P3).
//!
//! Split from [`crate::authz`] to respect the file-size budget: this
//! module owns only the idempotent default-policy seeding plus the
//! end-to-end authorization-chain tests that depend on it.

use std::sync::Arc;

use api::rbac::PolicyRule;
use storage::{Key, StorageBackend};

use crate::authz::RBAC_GROUP;

/// Idempotent bootstrap of the server's default RBAC policy (P3, mirrors
/// the Sprint-13 namespace bootstrap): `cluster-admin` (wildcard) bound to
/// `system:masters`, and `system:node` (wildcard — the kubelet plane needs
/// nodes/pods/leases/events/status writes; least-privilege carving is
/// T3.3 territory) bound to `system:nodes` so node tokens keep working
/// the moment `--token-file` arms the HTTP authz plane.
pub async fn bootstrap_rbac(store: &Arc<dyn StorageBackend>) {
    let wildcard = PolicyRule {
        verbs: vec!["*".into()],
        api_groups: vec!["*".into()],
        resources: vec!["*".into()],
        ..PolicyRule::default()
    };
    let cluster_admin = api::rbac::new_cluster_role("cluster-admin", vec![wildcard.clone()]);
    let system_node = api::rbac::new_cluster_role("system:node", vec![wildcard]);
    let masters_binding = api::rbac::new_group_cluster_role_binding(
        "cluster-admin",
        "system:masters",
        "cluster-admin",
    );
    let nodes_binding =
        api::rbac::new_group_cluster_role_binding("system:node", "system:nodes", "system:node");
    for (resource, name, value) in [
        (
            "clusterroles",
            "cluster-admin",
            serde_json::to_value(&cluster_admin).expect("json"),
        ),
        (
            "clusterroles",
            "system:node",
            serde_json::to_value(&system_node).expect("json"),
        ),
        (
            "clusterrolebindings",
            "cluster-admin",
            serde_json::to_value(&masters_binding).expect("json"),
        ),
        (
            "clusterrolebindings",
            "system:node",
            serde_json::to_value(&nodes_binding).expect("json"),
        ),
    ] {
        match store
            .create(&Key::new(RBAC_GROUP, resource, "", name), value)
            .await
        {
            Ok(_) => tracing::debug!(target: "init-pro", name, "bootstrapped rbac object"),
            Err(storage::StorageError::AlreadyExists { .. }) => {}
            Err(e) => tracing::warn!(target: "init-pro", name, error = %e, "rbac bootstrap failed"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authn::Identity;
    use crate::authz::{authorize, parse_target, Target};
    use axum::http::{Method, Uri};

    fn uri(p: &str) -> Uri {
        p.parse().unwrap()
    }

    #[tokio::test]
    async fn authorize_cluster_and_namespaced_chains() {
        let store: Arc<dyn StorageBackend> = Arc::new(storage::EmbeddedStorage::new());
        bootstrap_rbac(&store).await;
        let admin = Identity {
            user: "admin".into(),
            uid: "1".into(),
            groups: vec!["system:masters".into()],
        };
        let ok = parse_target(&Method::GET, &uri("/api/v1/namespaces/default/pods"));
        match &ok {
            Target::Resource(a) => assert!(authorize(&store, &admin, a).await),
            _ => panic!("resource"),
        }
        // system:nodes holds the wildcard system:node role too (P3).
        let node = Identity {
            user: "kubelet-n1".into(),
            uid: "2".into(),
            groups: vec!["system:nodes".into()],
        };
        match &ok {
            Target::Resource(a) => assert!(authorize(&store, &node, a).await),
            _ => panic!("resource"),
        }
        // A token with no group: cluster-admin binding exists but the
        // subject does not match -> deny.
        let nobody = Identity {
            user: "nobody".into(),
            uid: "3".into(),
            groups: vec![],
        };
        match &ok {
            Target::Resource(a) => assert!(!authorize(&store, &nobody, a).await),
            _ => panic!("resource"),
        }
    }

    #[tokio::test]
    async fn authorize_namespaced_role_scoping() {
        let store: Arc<dyn StorageBackend> = Arc::new(storage::EmbeddedStorage::new());
        // Role pods-reader in ns alpha only.
        let role = serde_json::json!({
            "apiVersion": "rbac.authorization.k8s.io/v1",
            "kind": "Role",
            "metadata": { "name": "pods-reader", "namespace": "alpha" },
            "rules": [{
                "verbs": ["get", "list"],
                "apiGroups": [""],
                "resources": ["pods"],
            }],
        });
        store
            .create(&Key::new(RBAC_GROUP, "roles", "alpha", "pods-reader"), role)
            .await
            .unwrap();
        let rb = serde_json::json!({
            "apiVersion": "rbac.authorization.k8s.io/v1",
            "kind": "RoleBinding",
            "metadata": { "name": "read", "namespace": "alpha" },
            "roleRef": {
                "apiGroup": RBAC_GROUP,
                "kind": "Role",
                "name": "pods-reader",
            },
            "subjects": [{
                "kind": "User",
                "apiGroup": RBAC_GROUP,
                "name": "alice",
            }],
        });
        store
            .create(&Key::new(RBAC_GROUP, "rolebindings", "alpha", "read"), rb)
            .await
            .unwrap();
        let alice = Identity {
            user: "alice".into(),
            uid: "9".into(),
            groups: vec![],
        };
        let alpha = parse_target(&Method::GET, &uri("/api/v1/namespaces/alpha/pods"));
        let beta = parse_target(&Method::GET, &uri("/api/v1/namespaces/beta/pods"));
        let nodes = parse_target(&Method::GET, &uri("/api/v1/nodes"));
        match (&alpha, &beta, &nodes) {
            (Target::Resource(a), Target::Resource(b), Target::Resource(c)) => {
                assert!(authorize(&store, &alice, a).await, "own namespace");
                assert!(!authorize(&store, &alice, b).await, "foreign namespace");
                assert!(!authorize(&store, &alice, c).await, "cluster-scoped");
            }
            _ => panic!("resource"),
        }
    }
}
