//! Reconcile-level tests for the M2 CI/CD pipeline controller
//! (TODO **T8.3**, decision **Q56**): tag -> `Build` child creation,
//! in-flight phase mirroring, promotion (target Deployment image patch),
//! the no-rebuild failure hold, the disabled plane and the Build phase
//! machine's terminal failure path.
//!
//! Storage is the real `EmbeddedStorage` behind `StorageClient`, so the
//! CAS/write-if-changed plumbing is exercised, not mocked.

use std::sync::Arc;

use serde_json::{json, Value};
use storage::{EmbeddedStorage, Key, KeyPrefix};

use crate::client::{Client, StorageClient};
use crate::object::resource_version;

use super::{build, reconcile, PipelineConfig};

type TestClient = Arc<dyn Client>;

fn client() -> TestClient {
    Arc::new(StorageClient::new(Arc::new(EmbeddedStorage::new())))
}

fn cfg() -> Option<PipelineConfig> {
    Some(PipelineConfig {
        registry_addr: "reg.local:5000".to_string(),
        scratch_dir: std::env::temp_dir().join("init-pro-pipeline-tests"),
        docker_bin: "false".to_string(),
        default_timeout_seconds: 5,
    })
}

fn sp_key() -> Key {
    Key::new("init-pro.io", "servicepipelines", "default", "web")
}

fn build_key(name: &str) -> Key {
    Key::new("init-pro.io", "builds", "default", name)
}

fn pipeline_json(tag: &str) -> Value {
    json!({
        "apiVersion": "init-pro.io/v1",
        "kind": "ServicePipeline",
        "metadata": {"name": "web", "namespace": "default"},
        "spec": {
            "image": {"repository": "team/web", "tag": tag},
            "source": {"context": "/src/web"},
            "target": {"deployment": "web", "container": "app"},
        },
    })
}

async fn create_pipeline(client: &TestClient, tag: &str) -> Value {
    client.create(&sp_key(), pipeline_json(tag)).await.unwrap()
}

async fn children(client: &TestClient) -> Vec<Value> {
    client
        .list(&KeyPrefix::new(
            "init-pro.io",
            "builds",
            Some("default".to_string()),
        ))
        .await
        .unwrap()
}

async fn stored_pipeline(client: &TestClient) -> Value {
    client.get(&sp_key()).await.unwrap().unwrap()
}

async fn stored_build(client: &TestClient, name: &str) -> Value {
    client.get(&build_key(name)).await.unwrap().unwrap()
}

/// Create a `Build` child owned by `sp` with the given phase.
async fn create_build(
    client: &TestClient,
    sp: &Value,
    name: &str,
    tag: &str,
    phase: &str,
) -> Value {
    let value = json!({
        "apiVersion": "init-pro.io/v1",
        "kind": "Build",
        "metadata": {
            "name": name,
            "namespace": "default",
            "ownerReferences": [{
                "apiVersion": "init-pro.io/v1",
                "kind": "ServicePipeline",
                "name": "web",
                "uid": sp["metadata"]["uid"],
                "controller": true,
            }],
        },
        "spec": {"image": {"repository": "team/web", "tag": tag}},
        "status": {"phase": phase, "image": format!("team/web:{tag}")},
    });
    client.create(&build_key(name), value).await.unwrap()
}

#[tokio::test]
async fn tag_transition_creates_exactly_one_owned_build() {
    let client = client();
    let sp = create_pipeline(&client, "v1").await;
    reconcile(&client, &sp, &cfg()).await.unwrap();

    let made = children(&client).await;
    assert_eq!(made.len(), 1, "one build per tag transition");
    let child = &made[0];
    let name = child["metadata"]["name"].as_str().unwrap();
    assert!(name.starts_with("web-v1-"), "name: {name}");
    assert_eq!(
        child["metadata"]["ownerReferences"][0]["kind"],
        json!("ServicePipeline")
    );
    assert_eq!(
        child["metadata"]["ownerReferences"][0]["name"],
        json!("web")
    );
    assert_eq!(
        child["metadata"]["ownerReferences"][0]["uid"],
        sp["metadata"]["uid"]
    );
    assert_eq!(
        child["metadata"]["labels"]["init-pro.io/pipeline"],
        json!("web")
    );
    assert_eq!(child["spec"]["image"]["tag"], json!("v1"));
    assert_eq!(child["spec"]["source"]["context"], json!("/src/web"));

    let stored = stored_pipeline(&client).await;
    assert_eq!(stored["status"]["phase"], json!("Building"));
    assert_eq!(stored["status"]["lastBuild"]["name"], json!(name));

    // Second pass observes the in-flight child: still exactly one build.
    reconcile(&client, &stored, &cfg()).await.unwrap();
    assert_eq!(children(&client).await.len(), 1);
}

#[tokio::test]
async fn in_flight_child_phase_is_mirrored() {
    let client = client();
    let sp = create_pipeline(&client, "v1").await;
    let child = create_build(&client, &sp, "web-v1-ab12c", "v1", "Publishing").await;
    reconcile(&client, &sp, &cfg()).await.unwrap();
    let stored = stored_pipeline(&client).await;
    assert_eq!(stored["status"]["phase"], json!("Publishing"));

    // Advance the child to a terminal phase: the pipeline promotes.
    let mut next = child.clone();
    next["status"]["phase"] = json!("Succeeded");
    next["status"]["digest"] = json!("sha256:abc");
    client
        .update(&build_key("web-v1-ab12c"), next, resource_version(&child))
        .await
        .unwrap();
    reconcile(&client, &stored, &cfg()).await.unwrap();
    let promoted = stored_pipeline(&client).await;
    assert_eq!(promoted["status"]["phase"], json!("Promoted"));
    assert_eq!(promoted["status"]["observedTag"], json!("v1"));
}

#[tokio::test]
async fn succeeded_child_promotes_and_patches_the_target_deployment() {
    let client = client();
    let sp = create_pipeline(&client, "v1").await;
    let dep_key = Key::new("apps", "deployments", "default", "web");
    client
        .create(
            &dep_key,
            json!({
                "apiVersion": "apps/v1",
                "kind": "Deployment",
                "metadata": {"name": "web", "namespace": "default"},
                "spec": {"template": {"spec": {"containers": [
                    {"name": "app", "image": "team/web:v0"},
                    {"name": "sidecar", "image": "envoy:v1"},
                ]}}},
            }),
        )
        .await
        .unwrap();
    let child = create_build(&client, &sp, "web-v1-ab12c", "v1", "Pending").await;
    let mut next = child.clone();
    next["status"]["phase"] = json!("Succeeded");
    client
        .update(&build_key("web-v1-ab12c"), next, resource_version(&child))
        .await
        .unwrap();

    reconcile(&client, &sp, &cfg()).await.unwrap();

    let stored = stored_pipeline(&client).await;
    assert_eq!(stored["status"]["phase"], json!("Promoted"));
    assert_eq!(stored["status"]["observedTag"], json!("v1"));
    assert_eq!(stored["status"]["lastBuild"]["name"], json!("web-v1-ab12c"));
    assert_eq!(stored["status"]["conditions"][0]["type"], json!("Ready"));
    assert_eq!(stored["status"]["conditions"][0]["status"], json!("True"));

    let dep = client.get(&dep_key).await.unwrap().unwrap();
    assert_eq!(
        dep["spec"]["template"]["spec"]["containers"][0]["image"],
        json!("team/web:v1"),
        "target container is patched to the built reference"
    );
    assert_eq!(
        dep["spec"]["template"]["spec"]["containers"][1]["image"],
        json!("envoy:v1"),
        "other containers are untouched"
    );

    // Idempotent: a promoted pipeline never rebuilds.
    reconcile(&client, &stored, &cfg()).await.unwrap();
    assert_eq!(children(&client).await.len(), 1);
}

#[tokio::test]
async fn failed_child_is_reported_without_rebuilding() {
    let client = client();
    let sp = create_pipeline(&client, "v1").await;
    create_build(&client, &sp, "web-v1-ab12c", "v1", "Failed").await;
    reconcile(&client, &sp, &cfg()).await.unwrap();

    let stored = stored_pipeline(&client).await;
    assert_eq!(stored["status"]["phase"], json!("Failed"));
    assert_eq!(stored["status"]["lastBuild"]["name"], json!("web-v1-ab12c"));
    assert_eq!(stored["status"]["conditions"][0]["status"], json!("False"));
    assert_eq!(
        stored["status"]["conditions"][0]["reason"],
        json!("BuildFailed")
    );
    assert_eq!(children(&client).await.len(), 1, "no rebuild loop");

    // A second pass holds the same report (write-if-changed: no churn).
    reconcile(&client, &stored, &cfg()).await.unwrap();
    assert_eq!(children(&client).await.len(), 1);
}

#[tokio::test]
async fn disabled_plane_reconciles_to_nothing() {
    let client = client();
    let sp = create_pipeline(&client, "v1").await;
    reconcile(&client, &sp, &None).await.unwrap();
    assert!(children(&client).await.is_empty());
    let stored = stored_pipeline(&client).await;
    assert_eq!(stored["status"], json!(null));
}

#[tokio::test]
async fn build_without_source_fails_terminally() {
    let client = client();
    let sp = create_pipeline(&client, "v1").await;
    let child = create_build(&client, &sp, "web-v1-ab12c", "v1", "Pending").await;
    build::reconcile(&client, &child, &cfg().unwrap())
        .await
        .unwrap();
    let stored = stored_build(&client, "web-v1-ab12c").await;
    assert_eq!(stored["status"]["phase"], json!("Failed"));
    let message = stored["status"]["conditions"][0]["message"]
        .as_str()
        .unwrap();
    assert!(message.contains("no source"), "message: {message}");
    assert_eq!(
        stored["status"]["conditions"][0]["type"],
        json!("Completed")
    );

    // Terminal builds are never re-run: the revision does not move.
    let revision = resource_version(&stored);
    build::reconcile(&client, &stored, &cfg().unwrap())
        .await
        .unwrap();
    assert_eq!(
        resource_version(&stored_build(&client, "web-v1-ab12c").await),
        revision
    );
}
