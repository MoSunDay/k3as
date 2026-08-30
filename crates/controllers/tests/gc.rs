//! T3.1b (slice S5) acceptance: garbage collection + namespace lifecycle
//! through the real `ControllerManager` (leader election + informers +
//! workqueues) against `EmbeddedStorage`. Decision **Q20**: background
//! cascade is the default (owner DELETE -> sweep), Orphan strips
//! ownerReferences, and terminating namespaces drain then finalize (the
//! terminal delete is owned here because in-process controllers bypass the
//! apiserver, Q19). Every wait is a poll with timeout -- NO fixed windows.

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use controllers::{Client, ControllerManager, Stop, StorageClient};
use serde_json::{json, Value};
use storage::{EmbeddedStorage, Key, KeyPrefix};

/// Poll `cond` (an async block) until true, panicking after `ms`.
macro_rules! eventually {
    ($ms:expr, $cond:expr) => {{
        let deadline = Instant::now() + Duration::from_millis($ms);
        loop {
            if $cond.await {
                break;
            }
            assert!(Instant::now() < deadline, "timed out waiting for condition");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }};
}

/// Local copy of the controllers.rs Env helper (integration-test pattern).
struct Env {
    client: StorageClient,
    _stop: Stop,
}

fn env() -> Env {
    let store = Arc::new(EmbeddedStorage::new());
    let stop = Stop::new();
    let _handles = ControllerManager::spawn(store.clone(), stop.clone());
    let client = StorageClient::new(store.clone());
    Env {
        client,
        _stop: stop,
    }
}

fn deployment(image: &str, replicas: u64) -> Value {
    json!({
        "apiVersion": "apps/v1",
        "kind": "Deployment",
        "metadata": {"name": "web", "namespace": "default"},
        "spec": {
            "replicas": replicas,
            "selector": {"matchLabels": {"app": "web"}},
            "template": {
                "metadata": {"labels": {"app": "web"}},
                "spec": {"containers": [{"name": "c", "image": image}]},
            },
        },
    })
}

async fn list(env: &Env, group: &str, resource: &str, ns: &str) -> Vec<Value> {
    env.client
        .list(&KeyPrefix::new(group, resource, Some(ns.to_string())))
        .await
        .unwrap()
}

async fn create(env: &Env, key: Key, obj: Value) {
    env.client.create(&key, obj).await.unwrap();
}

fn revision(v: &Value) -> u64 {
    v.pointer("/metadata/resourceVersion")
        .and_then(Value::as_str)
        .unwrap()
        .parse()
        .unwrap()
}

/// Stamp `deletionTimestamp` on the named Namespace (the apiserver's soft
/// delete, CAS-guarded) so the namespace controller starts draining.
async fn terminate(env: &Env, ns: &str) {
    let key = Key::new("", "namespaces", "", ns);
    let cur = env.client.get(&key).await.unwrap().unwrap();
    let mut terminating = cur.clone();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    terminating["metadata"]["deletionTimestamp"] = json!(controllers::time::now_rfc3339(now));
    env.client
        .update(&key, terminating, Some(revision(&cur)))
        .await
        .unwrap();
}

/// True once the Namespace object is gone (terminal delete) AND every
/// listed `(group, resource)` has no objects left under it.
async fn drained_and_finalized(env: &Env, ns: &str, kinds: &[(&str, &str)]) -> bool {
    if env
        .client
        .get(&Key::new("", "namespaces", "", ns))
        .await
        .unwrap()
        .is_some()
    {
        return false;
    }
    for (group, resource) in kinds {
        if !list(env, group, resource, ns).await.is_empty() {
            return false;
        }
    }
    true
}

/// Namespaced object with just enough shape for the controllers that
/// reconcile it (all ownerless unless a caller adds ownerReferences).
fn obj(api_version: &str, kind: &str, ns: &str, name: &str) -> Value {
    json!({
        "apiVersion": api_version,
        "kind": kind,
        "metadata": {"name": name, "namespace": ns},
    })
}

#[tokio::test]
async fn gc_cascades_deployment_delete_to_pods() {
    let env = env();
    create(
        &env,
        Key::new("apps", "deployments", "default", "web"),
        deployment("nginx:1.28", 2),
    )
    .await;
    eventually!(5000, async {
        list(&env, "", "pods", "default").await.len() == 2
    });

    // Owner delete (background cascade, Q20): RS AND pods must disappear.
    env.client
        .delete(&Key::new("apps", "deployments", "default", "web"))
        .await
        .unwrap();
    eventually!(15000, async {
        list(&env, "apps", "replicasets", "default")
            .await
            .is_empty()
            && list(&env, "", "pods", "default").await.is_empty()
    });
}

#[tokio::test]
async fn orphan_strips_owner_refs_and_keeps_replicaset() {
    let env = env();
    create(
        &env,
        Key::new("apps", "deployments", "default", "web"),
        deployment("nginx:1.28", 2),
    )
    .await;
    eventually!(5000, async {
        list(&env, "", "pods", "default").await.len() == 2
    });

    // Stamp the propagation marker (what the apiserver's
    // ?propagationPolicy=Orphan DELETE does) and delete the owner.
    let key = Key::new("apps", "deployments", "default", "web");
    let dep = env.client.get(&key).await.unwrap().unwrap();
    let mut orphaned = dep.clone();
    orphaned["metadata"]["annotations"]["init-pro.io/deletion-propagation"] = json!("Orphan");
    env.client
        .update(&key, orphaned, Some(revision(&dep)))
        .await
        .unwrap();
    env.client.delete(&key).await.unwrap();

    // The RS survives the owner delete: ownerReferences emptied, pods kept.
    eventually!(15000, async {
        let rs = list(&env, "apps", "replicasets", "default").await;
        rs.len() == 1
            && rs[0]
                .pointer("/metadata/ownerReferences")
                .and_then(Value::as_array)
                .is_some_and(|a| a.is_empty())
            && list(&env, "", "pods", "default").await.len() == 2
    });
}

#[tokio::test]
async fn dangling_owner_ref_pod_is_collected() {
    let env = env();
    let pod = json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {
            "name": "lonely",
            "namespace": "default",
            "ownerReferences": [
                {"kind": "ReplicaSet", "name": "ghost", "uid": "", "controller": true},
            ],
        },
        "spec": {"containers": [{"name": "c", "image": "nginx"}]},
    });
    create(&env, Key::new("", "pods", "default", "lonely"), pod).await;
    eventually!(10000, async {
        list(&env, "", "pods", "default").await.is_empty()
    });
}

#[tokio::test]
async fn namespace_termination_drains_and_finalizes() {
    let env = env();
    let ns_key = Key::new("", "namespaces", "", "ns-drain");
    create(
        &env,
        ns_key.clone(),
        json!({
            "apiVersion": "v1",
            "kind": "Namespace",
            "metadata": {"name": "ns-drain", "finalizers": ["kubernetes"]},
        }),
    )
    .await;
    create(
        &env,
        Key::new("", "configmaps", "ns-drain", "keepme"),
        json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": {"name": "keepme", "namespace": "ns-drain"},
            "data": {"k": "v"},
        }),
    )
    .await;
    let mut dep = deployment("nginx:1.28", 1);
    dep["metadata"]["namespace"] = json!("ns-drain");
    create(
        &env,
        Key::new("apps", "deployments", "ns-drain", "web"),
        dep,
    )
    .await;
    eventually!(5000, async {
        list(&env, "", "pods", "ns-drain").await.len() == 1
    });

    // Simulate the apiserver soft delete: stamp deletionTimestamp (CAS).
    terminate(&env, "ns-drain").await;

    // Drain -> finalize: every namespaced object AND the Namespace object
    // itself must disappear (the terminal delete, Q19/Q20).
    eventually!(20000, async {
        list(&env, "", "configmaps", "ns-drain").await.is_empty()
            && list(&env, "apps", "deployments", "ns-drain")
                .await
                .is_empty()
            && list(&env, "apps", "replicasets", "ns-drain")
                .await
                .is_empty()
            && list(&env, "", "pods", "ns-drain").await.is_empty()
            && env.client.get(&ns_key).await.unwrap().is_none()
    });
}

// --- Namespace drain closure (regression: 6 served kinds were orphaned) ---
//
// The drain table used to cover only the 12 core/apps/coordination kinds,
// so a terminating namespace finalized while its Events, Workflows,
// CronWorkflows, Agents, LuaRouters and Ingresses survived as orphans.
// These tests pin the closure: EVERY namespaced kind the apiserver serves
// must drain before the terminal delete (T3.1b, Q20).

/// The six kinds that used to leak past the drain (group, resource) --
/// group strings WITHOUT the version, exactly as `KeyPrefix` scopes keys.
const LEAKED_KINDS: [(&str, &str); 6] = [
    ("", "events"),
    ("argoproj.io", "workflows"),
    ("argoproj.io", "cronworkflows"),
    ("init-pro.io", "agents"),
    ("init-pro.io", "luarouters"),
    ("networking.k8s.io", "ingresses"),
];

/// A CronWorkflow that never fires (no schedule -> reconcile returns
/// early), so the drain is not racing child creation.
fn inert_cronworkflow(ns: &str, name: &str) -> Value {
    let mut cwf = obj("argoproj.io/v1alpha1", "CronWorkflow", ns, name);
    cwf["spec"] = json!({"suspend": true});
    cwf
}

/// A Workflow parked in a terminal phase: the workflow controller
/// converges (early-returns) without creating child objects.
fn terminal_workflow(ns: &str, name: &str) -> Value {
    let mut wf = obj("argoproj.io/v1alpha1", "Workflow", ns, name);
    wf["status"] = json!({"phase": "Succeeded"});
    wf
}

#[tokio::test]
async fn namespace_termination_drains_all_served_kinds() {
    let env = env();
    let ns = "ns-everything";
    create(
        &env,
        Key::new("", "namespaces", "", ns),
        json!({
            "apiVersion": "v1",
            "kind": "Namespace",
            "metadata": {"name": ns, "finalizers": ["kubernetes"]},
        }),
    )
    .await;
    // One object of each previously-leaked kind. The cronworkflow is
    // inert and the workflow terminal, so nothing recreates mid-drain;
    // the agent carries an ownerReference to prove OWNED objects drain
    // too (not just the ownerless shape).
    create(
        &env,
        Key::new("", "events", ns, "ev"),
        obj("v1", "Event", ns, "ev"),
    )
    .await;
    create(
        &env,
        Key::new("argoproj.io", "workflows", ns, "wf"),
        terminal_workflow(ns, "wf"),
    )
    .await;
    create(
        &env,
        Key::new("argoproj.io", "cronworkflows", ns, "cwf"),
        inert_cronworkflow(ns, "cwf"),
    )
    .await;
    let mut agent = obj("init-pro.io/v1", "Agent", ns, "ag");
    agent["metadata"]["ownerReferences"] = json!([{
        "apiVersion": "argoproj.io/v1alpha1",
        "kind": "Workflow",
        "name": "wf",
        "uid": "wf-uid",
        "controller": true,
    }]);
    create(&env, Key::new("init-pro.io", "agents", ns, "ag"), agent).await;
    create(
        &env,
        Key::new("init-pro.io", "luarouters", ns, "lr"),
        obj("init-pro.io/v1", "LuaRouter", ns, "lr"),
    )
    .await;
    create(
        &env,
        Key::new("networking.k8s.io", "ingresses", ns, "ing"),
        obj("networking.k8s.io/v1", "Ingress", ns, "ing"),
    )
    .await;

    terminate(&env, ns).await;
    eventually!(20000, async {
        drained_and_finalized(&env, ns, &LEAKED_KINDS).await
    });
}

#[tokio::test]
async fn namespace_termination_drains_ownerless_kinds() {
    let env = env();
    let ns = "ns-ownerless";
    create(
        &env,
        Key::new("", "namespaces", "", ns),
        json!({
            "apiVersion": "v1",
            "kind": "Namespace",
            "metadata": {"name": ns, "finalizers": ["kubernetes"]},
        }),
    )
    .await;
    // The exact leak shape: only kinds the GC cascade can NEVER reach
    // (no controllerRef/owner anywhere), all from the leaked set.
    create(
        &env,
        Key::new("", "events", ns, "orphan-ev"),
        obj("v1", "Event", ns, "orphan-ev"),
    )
    .await;
    create(
        &env,
        Key::new("init-pro.io", "luarouters", ns, "orphan-lr"),
        obj("init-pro.io/v1", "LuaRouter", ns, "orphan-lr"),
    )
    .await;
    create(
        &env,
        Key::new("networking.k8s.io", "ingresses", ns, "orphan-ing"),
        obj("networking.k8s.io/v1", "Ingress", ns, "orphan-ing"),
    )
    .await;
    create(
        &env,
        Key::new("argoproj.io", "workflows", ns, "orphan-wf"),
        terminal_workflow(ns, "orphan-wf"),
    )
    .await;

    terminate(&env, ns).await;
    // Without the drain covering these four, the Namespace would finalize
    // and leave them behind; with it, the terminal delete is complete.
    eventually!(20000, async {
        drained_and_finalized(
            &env,
            ns,
            &[
                ("", "events"),
                ("init-pro.io", "luarouters"),
                ("networking.k8s.io", "ingresses"),
                ("argoproj.io", "workflows"),
            ],
        )
        .await
    });
}

#[tokio::test]
async fn namespace_termination_drains_core_workload_kinds_and_empty_ns_finalizes() {
    let env = env();
    let ns = "ns-legacy";
    create(
        &env,
        Key::new("", "namespaces", "", ns),
        json!({
            "apiVersion": "v1",
            "kind": "Namespace",
            "metadata": {"name": ns, "finalizers": ["kubernetes"]},
        }),
    )
    .await;
    // No-regression: every kind the table has always covered.
    create(&env, Key::new("", "pods", ns, "p1"), {
        let mut pod = obj("v1", "Pod", ns, "p1");
        pod["spec"] = json!({"containers": [{"name": "c", "image": "busybox"}]});
        pod
    })
    .await;
    create(&env, Key::new("", "services", ns, "svc1"), {
        let mut svc = obj("v1", "Service", ns, "svc1");
        svc["spec"] = json!({"ports": [{"port": 80}]});
        svc
    })
    .await;
    create(
        &env,
        Key::new("", "endpoints", ns, "ep1"),
        obj("v1", "Endpoints", ns, "ep1"),
    )
    .await;
    create(&env, Key::new("", "configmaps", ns, "cm1"), {
        let mut cm = obj("v1", "ConfigMap", ns, "cm1");
        cm["data"] = json!({"k": "v"});
        cm
    })
    .await;
    create(&env, Key::new("", "secrets", ns, "s1"), {
        let mut sec = obj("v1", "Secret", ns, "s1");
        sec["data"] = json!({"k": "dg=="}); // base64("r") -- test fixture
        sec
    })
    .await;
    create(&env, Key::new("", "persistentvolumeclaims", ns, "pvc1"), {
        let mut pvc = obj("v1", "PersistentVolumeClaim", ns, "pvc1");
        pvc["spec"] = json!({"accessModes": ["ReadWriteOnce"]});
        pvc
    })
    .await;
    let mut dep = deployment("nginx:1.28", 1);
    dep["metadata"]["namespace"] = json!(ns);
    create(&env, Key::new("apps", "deployments", ns, "web"), dep).await;
    create(&env, Key::new("apps", "replicasets", ns, "rs1"), {
        let mut rs = obj("apps/v1", "ReplicaSet", ns, "rs1");
        rs["spec"] = json!({"replicas": 0, "selector": {"matchLabels": {"app": "rs1"}}});
        rs
    })
    .await;
    create(&env, Key::new("apps", "statefulsets", ns, "sts1"), {
        let mut sts = obj("apps/v1", "StatefulSet", ns, "sts1");
        sts["spec"] = json!({"replicas": 0, "serviceName": "svc1"});
        sts
    })
    .await;
    create(&env, Key::new("apps", "daemonsets", ns, "ds1"), {
        let mut ds = obj("apps/v1", "DaemonSet", ns, "ds1");
        ds["spec"] = json!({"selector": {"matchLabels": {"app": "ds1"}}});
        ds
    })
    .await;
    create(&env, Key::new("apps", "controllerrevisions", ns, "cr1"), {
        let mut cr = obj("apps/v1", "ControllerRevision", ns, "cr1");
        cr["revision"] = json!(1);
        cr
    })
    .await;
    create(&env, Key::new("coordination.k8s.io", "leases", ns, "l1"), {
        let mut lease = obj("coordination.k8s.io/v1", "Lease", ns, "l1");
        lease["spec"] = json!({"holderIdentity": "test"});
        lease
    })
    .await;

    terminate(&env, ns).await;
    eventually!(20000, async {
        drained_and_finalized(
            &env,
            ns,
            &[
                ("", "pods"),
                ("", "services"),
                ("", "endpoints"),
                ("", "configmaps"),
                ("", "secrets"),
                ("", "persistentvolumeclaims"),
                ("apps", "deployments"),
                ("apps", "replicasets"),
                ("apps", "statefulsets"),
                ("apps", "daemonsets"),
                ("apps", "controllerrevisions"),
                ("coordination.k8s.io", "leases"),
            ],
        )
        .await
    });

    // An EMPTY namespace finalizes in a single pass: with nothing to drain
    // the very first reconcile reaches the terminal delete (any leftover
    // kind would keep `drained` false forever and this would time out).
    let empty = "ns-empty";
    create(
        &env,
        Key::new("", "namespaces", "", empty),
        json!({
            "apiVersion": "v1",
            "kind": "Namespace",
            "metadata": {"name": empty, "finalizers": ["kubernetes"]},
        }),
    )
    .await;
    terminate(&env, empty).await;
    eventually!(20000, async { drained_and_finalized(&env, empty, &[]).await });
}
