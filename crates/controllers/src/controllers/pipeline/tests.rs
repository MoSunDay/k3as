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
use crate::error::ControllerError;
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
        max_image_bytes: registry::DEFAULT_MAX_IMAGE_BYTES,
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

/// Canary variant of [`pipeline_json`] (T8.5/Q58): `spec.release` pointing at
/// the stable pair the test seeds.
fn canary_pipeline_json(tag: &str) -> Value {
    let mut value = pipeline_json(tag);
    value["spec"]["release"] = json!({
        "trafficSplit": "web",
        "stableService": "web-stable",
        "steps": [{"weight": 100, "pauseSeconds": 600}],
    });
    value
}

#[tokio::test]
async fn canary_release_rolls_a_canary_instead_of_patching_stable() {
    let client = client();
    let dep_key = Key::new("apps", "deployments", "default", "web");
    let svc_key = Key::new("", "services", "default", "web-stable");
    client
        .create(
            &dep_key,
            json!({
                "apiVersion": "apps/v1",
                "kind": "Deployment",
                "metadata": {"name": "web", "namespace": "default"},
                "spec": {
                    "selector": {"matchLabels": {"app": "web"}},
                    "template": {
                        "metadata": {"labels": {"app": "web"}},
                        "spec": {"containers": [{"name": "app", "image": "team/web:v0"}]},
                    },
                },
            }),
        )
        .await
        .unwrap();
    client
        .create(
            &svc_key,
            json!({
                "apiVersion": "v1",
                "kind": "Service",
                "metadata": {"name": "web-stable", "namespace": "default"},
                "spec": {"selector": {"app": "web"}, "ports": [{"port": 80}]},
            }),
        )
        .await
        .unwrap();
    client
        .create(
            &Key::new("init-pro.io", "trafficsplits", "default", "web"),
            json!({
                "apiVersion": "init-pro.io/v1",
                "kind": "TrafficSplit",
                "metadata": {"name": "web", "namespace": "default"},
                "spec": {"service": "web-entry", "backends": [{"service": "web-stable", "weight": 100}]},
            }),
        )
        .await
        .unwrap();
    let sp = client
        .create(&sp_key(), canary_pipeline_json("v1"))
        .await
        .unwrap();
    create_build(&client, &sp, "web-v1-ab12c", "v1", "Succeeded").await;

    reconcile(&client, &sp, &cfg()).await.unwrap();

    // The canary rolls; the stable image is NOT patched (promotion owns it).
    let stored = stored_pipeline(&client).await;
    assert_eq!(stored["status"]["phase"], json!("Rolling"));
    assert_eq!(stored["status"]["rollout"]["step"], json!(0));
    assert_eq!(stored["status"]["observedTag"], json!(null));
    let canary_key = Key::new("apps", "deployments", "default", "web-canary");
    let canary = client.get(&canary_key).await.unwrap().unwrap();
    assert_eq!(
        canary["spec"]["template"]["spec"]["containers"][0]["image"],
        json!("team/web:v1")
    );
    let stable = client.get(&dep_key).await.unwrap().unwrap();
    assert_eq!(
        stable["spec"]["template"]["spec"]["containers"][0]["image"],
        json!("team/web:v0"),
        "the stable Deployment is untouched while rolling"
    );
    let canary_svc = Key::new("", "services", "default", "web-stable-canary");
    assert!(client.get(&canary_svc).await.unwrap().is_some());
}

#[tokio::test]
async fn deleting_pipeline_creates_no_build_and_writes_no_status() {
    let client = client();
    let mut sp = create_pipeline(&client, "v1").await;
    sp["metadata"]["deletionTimestamp"] = json!("2026-01-01T00:00:00Z");
    reconcile(&client, &sp, &cfg()).await.unwrap();

    assert!(
        children(&client).await.is_empty(),
        "a draining pipeline builds nothing"
    );
    assert_eq!(stored_pipeline(&client).await["status"], json!(null));
}

/// B8 wiring: the retention pass runs BEFORE `docker build`, selects images
/// by the `init-pro.pipeline` label, and its own failure is never fatal (the
/// Build fails on `docker build`, exactly as before). `docker` here is a
/// recording `/bin/sh` shim -- no daemon is involved.
#[tokio::test]
async fn build_prunes_pipeline_images_before_building() {
    use std::os::unix::fs::PermissionsExt;

    let dir = std::env::temp_dir().join(format!("init-pro-b8-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let calls = dir.join("docker-calls.log");
    let script = dir.join("docker");
    std::fs::write(
        &script,
        format!("#!/bin/sh\necho \"$*\" >> {}\nexit 1\n", calls.display()),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

    let client = client();
    let sp = create_pipeline(&client, "v1").await;
    let mut child = create_build(&client, &sp, "web-v1-ab12c", "v1", "Pending").await;
    child["spec"]["source"]["context"] = json!("/src/web");
    child["spec"]["pipeline"] = json!("web");

    let mut cfg = cfg().unwrap();
    cfg.docker_bin = script.display().to_string();
    build::reconcile(&client, &child, &cfg).await.unwrap();

    let recorded = std::fs::read_to_string(&calls).unwrap();
    let lines: Vec<&str> = recorded.lines().collect();
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("images --filter label=init-pro.pipeline=default/web")),
        "the retention pass lists the pipeline's images first: {lines:?}"
    );
    // The build itself still runs, labeled with the same pipeline identity.
    let build_call = lines
        .iter()
        .find(|l| l.starts_with("build "))
        .expect("docker build runs");
    assert!(
        build_call.contains("--label init-pro.pipeline=default/web"),
        "build call: {build_call}"
    );
    // Nothing to prune (the shim printed no rows) -> no `rmi` at all.
    assert!(!lines.iter().any(|l| l.starts_with("rmi")), "{lines:?}");

    // The retention pass never fails the Build: the terminal failure is the
    // docker build's own.
    let stored = stored_build(&client, "web-v1-ab12c").await;
    assert_eq!(stored["status"]["phase"], json!("Failed"));
    let message = stored["status"]["conditions"][0]["message"]
        .as_str()
        .unwrap();
    assert!(
        message.starts_with("docker build failed"),
        "message: {message}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- B10: retryable transport-level push failure --------------------------

/// A minimal plain (uncompressed) `docker save` archive, as the shim
/// produces -- enough for the importer to stage one layer.
fn write_save_fixture(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "init-pro-pipeline-retry-{tag}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let manifest = br#"[{"Config":"cfg.json","RepoTags":["team/web:r1"],"Layers":["layer.tar"]}]"#;
    let config = br#"{"architecture":"amd64","os":"linux"}"#;
    let mut builder = tar::Builder::new(Vec::new());
    for (name, data) in [
        ("manifest.json", &manifest[..]),
        ("cfg.json", &config[..]),
        ("layer.tar", b"layer-bytes"),
    ] {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Regular);
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        header.set_mtime(0);
        header.set_cksum();
        builder.append_data(&mut header, name, data).unwrap();
    }
    let path = dir.join("fixture.tar");
    std::fs::write(&path, builder.into_inner().unwrap()).unwrap();
    path
}

/// B10: a refused registry connection is a transport failure, not a verdict
/// on the image -- the Build stays `Publishing` with `status.pushRetries`
/// bumped and reconcile answers `Internal` (rate-limited requeue) until the
/// cap, then finalizes `Failed` exactly once.
#[tokio::test]
async fn push_transport_failure_retries_until_the_cap() {
    let fixture = write_save_fixture("b10");
    let client = client();
    let cfg = PipelineConfig {
        registry_addr: "127.0.0.1:1".to_string(), // refused loopback port
        scratch_dir: std::env::temp_dir().join("init-pro-pipeline-retry-scratch"),
        docker_bin: "false".to_string(), // archive mode: docker is never run
        default_timeout_seconds: 5,
        max_image_bytes: registry::DEFAULT_MAX_IMAGE_BYTES,
    };
    let key = build_key("web-retry");
    let build = json!({
        "apiVersion": "init-pro.io/v1",
        "kind": "Build",
        "metadata": {"name": "web-retry", "namespace": "default"},
        "spec": {
            "image": {"repository": "team/web", "tag": "r1"},
            "source": {"archive": fixture.display().to_string()},
        },
        "status": {"phase": "Publishing", "image": "team/web:r1"},
    });
    client.create(&key, build).await.unwrap();

    for attempt in 1..=2u64 {
        let err = build::reconcile(&client, &stored_build(&client, "web-retry").await, &cfg)
            .await
            .expect_err("transport failure must requeue, not finalize");
        assert!(matches!(err, ControllerError::Internal(_)), "{err:?}");
        assert!(
            err.to_string().contains(&format!("retry {attempt}/5")),
            "{err}"
        );
        let stored = stored_build(&client, "web-retry").await;
        assert_eq!(stored["status"]["phase"], json!("Publishing"));
        assert_eq!(stored["status"]["pushRetries"], json!(attempt));
        let tail = stored["status"]["logTail"].as_str().unwrap();
        assert!(
            tail.contains(&format!("push attempt {attempt}/5 failed")),
            "{tail}"
        );
    }

    // Cap reached: the next transport failure is terminal, with the count
    // in the condition message, and the terminal Build is never re-run.
    let mut exhausted = stored_build(&client, "web-retry").await;
    exhausted["status"]["pushRetries"] = json!(5);
    let exhausted = client
        .update(&key, exhausted.clone(), resource_version(&exhausted))
        .await
        .unwrap();
    build::reconcile(&client, &exhausted, &cfg).await.unwrap();
    let failed = stored_build(&client, "web-retry").await;
    assert_eq!(failed["status"]["phase"], json!("Failed"));
    let message = failed["status"]["conditions"][0]["message"]
        .as_str()
        .unwrap();
    assert!(message.contains("after 5 transport retries"), "{message}");
    let revision = resource_version(&failed);
    build::reconcile(&client, &failed, &cfg).await.unwrap();
    assert_eq!(
        resource_version(&stored_build(&client, "web-retry").await),
        revision
    );
    let _ = std::fs::remove_dir_all(fixture.parent().unwrap());
}
