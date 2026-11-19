//! Shared fixtures for the canary storage tests (TODO **T8.5**, Q58): the
//! stable Deployment/Service pair, the entry `TrafficSplit`, the pipeline
//! shape, one [`roll_once`] pass and the small readers the assertions use.

use std::sync::Arc;

use serde_json::{json, Value};
use storage::{EmbeddedStorage, Key};

use crate::client::{Client, StorageClient};

use super::super::roll;

pub(super) type TestClient = Arc<dyn Client>;

pub(super) fn client() -> TestClient {
    Arc::new(StorageClient::new(Arc::new(EmbeddedStorage::new())))
}

fn key(group: &str, resource: &str, name: &str) -> Key {
    Key::new(group, resource, "default", name)
}

pub(super) fn sp_key() -> Key {
    key("init-pro.io", "servicepipelines", "web")
}

pub(super) fn stable_dep_key() -> Key {
    key("apps", "deployments", "web")
}

pub(super) fn canary_dep_key() -> Key {
    key("apps", "deployments", "web-canary")
}

pub(super) fn canary_svc_key() -> Key {
    key("", "services", "web-stable-canary")
}

pub(super) fn split_key() -> Key {
    key("init-pro.io", "trafficsplits", "web")
}

pub(super) fn stable_deployment() -> Value {
    json!({
        "apiVersion": "apps/v1",
        "kind": "Deployment",
        "metadata": {"name": "web", "namespace": "default"},
        "spec": {
            "replicas": 2,
            "selector": {"matchLabels": {"app": "web"}},
            "template": {
                "metadata": {"labels": {"app": "web", "tier": "front"}},
                "spec": {"containers": [{"name": "app", "image": "team/web:v1"}]},
            },
        },
    })
}

pub(super) fn stable_service() -> Value {
    json!({
        "apiVersion": "v1",
        "kind": "Service",
        "metadata": {"name": "web-stable", "namespace": "default"},
        "spec": {
            "type": "NodePort",
            "selector": {"app": "web"},
            "ports": [{"name": "http", "port": 80, "targetPort": 8080, "nodePort": 30080}],
        },
    })
}

pub(super) fn traffic_split() -> Value {
    json!({
        "apiVersion": "init-pro.io/v1",
        "kind": "TrafficSplit",
        "metadata": {"name": "web", "namespace": "default"},
        "spec": {"service": "web-entry", "backends": [{"service": "web-stable", "weight": 100}]},
    })
}

pub(super) fn pipeline(steps: Value) -> Value {
    json!({
        "apiVersion": "init-pro.io/v1",
        "kind": "ServicePipeline",
        "metadata": {"name": "web", "namespace": "default"},
        "spec": {
            "image": {"repository": "team/web", "tag": "v2"},
            "source": {"context": "/src/web"},
            "target": {"deployment": "web", "container": "app"},
            "release": {"trafficSplit": "web", "stableService": "web-stable", "steps": steps},
        },
    })
}

pub(super) fn succeeded_build() -> Value {
    json!({
        "apiVersion": "init-pro.io/v1",
        "kind": "Build",
        "metadata": {"name": "web-v2-ab12c", "namespace": "default"},
        "spec": {"image": {"repository": "team/web", "tag": "v2"}},
        "status": {"phase": "Succeeded", "image": "team/web:v2"},
    })
}

pub(super) async fn stored(client: &TestClient, key: &Key) -> Value {
    client.get(key).await.unwrap().unwrap_or(Value::Null)
}

/// Seed the stable pair + the entry `TrafficSplit` + the pipeline.
pub(super) async fn seed(client: &TestClient, steps: Value) -> Value {
    client
        .create(&stable_dep_key(), stable_deployment())
        .await
        .unwrap();
    client
        .create(&key("", "services", "web-stable"), stable_service())
        .await
        .unwrap();
    client.create(&split_key(), traffic_split()).await.unwrap();
    client.create(&sp_key(), pipeline(steps)).await.unwrap()
}

/// One `roll` pass for tag `v2`, exactly as the reconcile would call it.
pub(super) async fn roll_once(client: &TestClient, sp: &Value) {
    let uid = sp
        .pointer("/metadata/uid")
        .and_then(Value::as_str)
        .unwrap_or("");
    roll(
        client,
        &sp_key(),
        sp,
        &succeeded_build(),
        "default",
        "web",
        uid,
        "v2",
        "team/web:v2",
    )
    .await
    .unwrap()
}

pub(super) fn image(object: &Value) -> Option<&str> {
    object
        .pointer("/spec/template/spec/containers/0/image")
        .and_then(Value::as_str)
}

/// `TrafficSplit` weight of `service` (0 when the backend is not there at
/// all -- a dropped backend and a zero weight assert the same).
pub(super) fn weight(split: &Value, service: &str) -> i32 {
    split
        .pointer("/spec/backends")
        .and_then(Value::as_array)
        .and_then(|backends| {
            backends
                .iter()
                .find(|b| b.get("service").and_then(Value::as_str) == Some(service))
        })
        .and_then(|b| b.get("weight"))
        .and_then(Value::as_i64)
        .unwrap_or(0) as i32
}

pub(super) fn reason(sp: &Value) -> String {
    sp.pointer("/status/conditions/0/reason")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}
