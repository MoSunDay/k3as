//! Storage-level tests for the canary release controller (TODO **T8.5**,
//! decision **Q58**): one `roll`/`abort` pass against the real
//! `EmbeddedStorage`, so the get/create-CAS plumbing is exercised, not
//! mocked. The full manager + registry flow lives in `tests/canary.rs`.

mod fixtures;

use serde_json::{json, Value};

use crate::object::resource_version;

use self::fixtures::*;
use super::abort;

#[tokio::test]
async fn roll_clones_the_stable_pair_and_publishes_step_weight() {
    let client = client();
    let sp = seed(&client, json!([{"weight": 30, "pauseSeconds": 600}])).await;
    roll_once(&client, &sp).await;

    let canary = stored(&client, &canary_dep_key()).await;
    assert_eq!(
        image(&canary),
        Some("team/web:v2"),
        "canary ships the revision"
    );
    assert_eq!(canary["spec"]["replicas"], json!(2), "replicas cloned");
    assert_eq!(
        canary["spec"]["selector"]["matchLabels"],
        json!({"init-pro.io/pipeline": "web", "init-pro.io/canary": "true"})
    );
    // The immutable stable selector key must not leak into the pod labels.
    assert!(canary["spec"]["template"]["metadata"]["labels"]
        .get("app")
        .is_none());
    assert_eq!(
        canary["metadata"]["ownerReferences"][0]["kind"],
        json!("ServicePipeline")
    );
    assert_eq!(
        canary["metadata"]["ownerReferences"][0]["uid"],
        sp["metadata"]["uid"]
    );

    let svc = stored(&client, &canary_svc_key()).await;
    assert_eq!(svc["spec"]["type"], json!("ClusterIP"));
    assert_eq!(svc["spec"]["ports"][0]["port"], json!(80));
    assert!(
        svc["spec"]["ports"][0].get("nodePort").is_none(),
        "the node port stays on the entry Service"
    );

    let split = stored(&client, &split_key()).await;
    assert_eq!(weight(&split, "web-stable"), 70);
    assert_eq!(weight(&split, "web-stable-canary"), 30);

    let rolling = stored(&client, &sp_key()).await;
    assert_eq!(rolling["status"]["phase"], json!("Rolling"));
    assert_eq!(rolling["status"]["rollout"]["step"], json!(0));
    assert_eq!(rolling["status"]["rollout"]["weight"], json!(30));
    assert_eq!(
        rolling["status"]["rollout"]["revision"],
        json!("team/web:v2")
    );
    assert_eq!(
        image(&stored(&client, &stable_dep_key()).await),
        Some("team/web:v1"),
        "the stable Deployment is untouched while rolling"
    );
}

#[tokio::test]
async fn soaking_pass_writes_nothing() {
    let client = client();
    let sp = seed(&client, json!([{"weight": 30, "pauseSeconds": 600}])).await;
    roll_once(&client, &sp).await;
    let rolling = stored(&client, &sp_key()).await;

    // Same revision, still soaking: `Wait` must not churn the status.
    let revision = resource_version(&rolling);
    roll_once(&client, &rolling).await;
    assert_eq!(
        resource_version(&stored(&client, &sp_key()).await),
        revision
    );
    let split = stored(&client, &split_key()).await;
    assert_eq!(weight(&split, "web-stable-canary"), 30);
}

#[tokio::test]
async fn foreign_canary_deployment_fails_without_adoption() {
    let client = client();
    let sp = seed(&client, json!([{"weight": 100, "pauseSeconds": 0}])).await;
    let foreign = client
        .create(
            &canary_dep_key(),
            json!({
                "apiVersion": "apps/v1",
                "kind": "Deployment",
                "metadata": {"name": "web-canary", "namespace": "default"},
                "spec": {},
            }),
        )
        .await
        .unwrap();
    roll_once(&client, &sp).await;

    let failed = stored(&client, &sp_key()).await;
    assert_eq!(failed["status"]["phase"], json!("Failed"));
    assert_eq!(reason(&failed), "CanaryConflict");
    // Never adopted, never rewritten; no canary Service either.
    let untouched = stored(&client, &canary_dep_key()).await;
    assert_eq!(resource_version(&untouched), resource_version(&foreign));
    assert!(stored(&client, &canary_svc_key()).await.is_null());
    assert_eq!(
        weight(&stored(&client, &split_key()).await, "web-stable"),
        100
    );
}

#[tokio::test]
async fn missing_traffic_split_fails_the_release() {
    let client = client();
    let sp = seed(&client, json!([{"weight": 100, "pauseSeconds": 0}])).await;
    client.delete(&split_key()).await.unwrap();
    roll_once(&client, &sp).await;

    let failed = stored(&client, &sp_key()).await;
    assert_eq!(failed["status"]["phase"], json!("Failed"));
    assert_eq!(reason(&failed), "TrafficSplitMissing");
    assert!(
        stored(&client, &canary_dep_key()).await.is_null(),
        "no children on a failed validation"
    );
}

#[tokio::test]
async fn promote_patches_the_stable_image_after_the_last_soak() {
    let client = client();
    let sp = seed(&client, json!([{"weight": 100, "pauseSeconds": 0}])).await;
    // Pretend step 0 was published long ago: its soak has elapsed.
    let mut seeded = sp.clone();
    seeded["status"] = json!({"phase": "Rolling", "rollout": {
        "step": 0, "weight": 100, "revision": "team/web:v2", "since": "2020-01-01T00:00:00Z",
    }});
    let seeded = client
        .update(&sp_key(), seeded, resource_version(&sp))
        .await
        .unwrap();
    roll_once(&client, &seeded).await;

    let promoted = stored(&client, &sp_key()).await;
    assert_eq!(promoted["status"]["phase"], json!("Promoted"));
    assert_eq!(promoted["status"]["observedTag"], json!("v2"));
    assert_eq!(
        promoted["status"]["lastBuild"]["name"],
        json!("web-v2-ab12c")
    );
    assert!(
        promoted["status"].get("rollout").is_none(),
        "no rollout in flight"
    );
    assert_eq!(reason(&promoted), "Promoted");
    assert_eq!(
        image(&stored(&client, &stable_dep_key()).await),
        Some("team/web:v2"),
        "promotion patches the stable image"
    );
    let split = stored(&client, &split_key()).await;
    assert_eq!(weight(&split, "web-stable"), 100);
    assert_eq!(weight(&split, "web-stable-canary"), 0);
}

/// Call [`abort`] with the identity the controller passes.
async fn abort_once(client: &TestClient, sp: &Value) {
    let uid = sp
        .pointer("/metadata/uid")
        .and_then(Value::as_str)
        .unwrap_or("");
    abort(client, &sp_key(), sp, "default", "web", uid)
        .await
        .unwrap()
}

#[tokio::test]
async fn abort_deletes_the_children_and_resets_the_split() {
    let client = client();
    let sp = seed(&client, json!([{"weight": 50, "pauseSeconds": 600}])).await;
    roll_once(&client, &sp).await;
    assert!(!stored(&client, &canary_dep_key()).await.is_null());

    let rolling = stored(&client, &sp_key()).await;
    let mut aborting = rolling.clone();
    aborting["spec"]["release"]["abort"] = json!(true);
    let aborting = client
        .update(&sp_key(), aborting, resource_version(&rolling))
        .await
        .unwrap();
    abort_once(&client, &aborting).await;

    assert!(
        stored(&client, &canary_dep_key()).await.is_null(),
        "canary Deployment deleted"
    );
    assert!(
        stored(&client, &canary_svc_key()).await.is_null(),
        "canary Service deleted"
    );
    let split = stored(&client, &split_key()).await;
    assert_eq!(weight(&split, "web-stable"), 100);
    assert_eq!(weight(&split, "web-stable-canary"), 0);
    let aborted = stored(&client, &sp_key()).await;
    assert_eq!(aborted["status"]["phase"], json!("Aborted"));
    assert_eq!(reason(&aborted), "Aborted");
    assert!(
        aborted["status"].get("observedTag").is_none(),
        "an abort never promotes"
    );
    assert_eq!(
        image(&stored(&client, &stable_dep_key()).await),
        Some("team/web:v1"),
        "the stable image is never touched"
    );

    // Aborting twice converges without a status write (anti-churn).
    let revision = resource_version(&aborted);
    abort_once(&client, &aborted).await;
    assert_eq!(
        resource_version(&stored(&client, &sp_key()).await),
        revision
    );
}
