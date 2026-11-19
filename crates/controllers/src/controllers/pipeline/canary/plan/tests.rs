//! Pure-decision tests for the canary release (TODO **T8.5**, Q58): weight
//! complement/clamp, label scoping, the step matrix, the child clones and
//! the name validation.

use super::*;

fn pipeline(tag: &str) -> Value {
    json!({
        "metadata": {"name": "web", "namespace": "default"},
        "spec": {
            "image": {"repository": "team/web", "tag": tag},
            "source": {"context": "/src/web"},
            "target": {"deployment": "web", "container": "app"},
            "release": {
                "trafficSplit": "web",
                "stableService": "web",
                "steps": [
                    {"weight": 10, "pauseSeconds": 30},
                    {"weight": 50, "pauseSeconds": 0},
                    {"weight": 100},
                ],
            },
        },
    })
}

fn plan_for(tag: &str) -> ReleasePlan {
    let sp = pipeline(tag);
    let release = parse_release(&sp).expect("release parses");
    let reference = format!("team/web:{tag}");
    validate(&sp, &release, &reference, tag).expect("valid release")
}

fn labels() -> Map<String, Value> {
    let pod_labels = json!({"app": "web", "tier": "backend"});
    let selector = json!({"app": "web"});
    canary_labels("web", &pod_labels, &selector, "v2")
}

fn owner() -> Value {
    json!({"apiVersion": "init-pro.io/v1", "kind": "ServicePipeline",
           "name": "web", "uid": "uid-web", "controller": true})
}

fn stable_deployment() -> Value {
    json!({
        "metadata": {
            "name": "web", "namespace": "default", "uid": "dep-uid",
            "resourceVersion": "9", "annotations": {"note": "stable"},
        },
        "spec": {
            "replicas": 3,
            "selector": {"matchLabels": {"app": "web"}},
            "template": {
                "metadata": {"labels": {"app": "web", "tier": "backend"}},
                "spec": {"containers": [{"name": "app", "image": "team/web:v1"}]},
            },
        },
        "status": {"readyReplicas": 3},
    })
}

fn stable_service() -> Value {
    json!({
        "metadata": {"name": "web", "namespace": "default", "resourceVersion": "4"},
        "spec": {
            "type": "NodePort",
            "selector": {"app": "web"},
            "ports": [{"name": "http", "port": 80, "targetPort": 8080,
                       "nodePort": 30080, "protocol": "TCP"}],
            "clusterIP": "10.43.1.2",
            "clusterIPs": ["10.43.1.2"],
            "ipFamilies": ["IPv4"],
            "ipFamilyPolicy": "SingleStack",
            "healthCheckNodePort": 30081,
            "externalTrafficPolicy": "Local",
            "sessionAffinity": "ClientIP",
        },
    })
}

fn validate_err(sp: &Value) -> String {
    let release = parse_release(sp).expect("release object");
    validate(sp, &release, "team/web:v2", "v2").expect_err("must be rejected")
}

#[test]
fn backends_complement_the_canary_weight_and_clamp() {
    let plan = plan_for("v2");
    let weights = |weight: i32| -> Vec<(String, i32)> {
        desired_backends(&plan, weight)
            .into_iter()
            .map(|b| (b.service, b.weight))
            .collect()
    };
    let stable = |weight: i32| vec![("web".to_string(), weight)];
    let canary = |weight: i32| vec![("web-canary".to_string(), weight)];
    // Order is stable-then-canary; the weights always sum to 100.
    assert_eq!(weights(0), [stable(100), canary(0)].concat());
    assert_eq!(weights(50), [stable(50), canary(50)].concat());
    assert_eq!(weights(100), [stable(0), canary(100)].concat());
    assert_eq!(
        weights(150),
        [stable(0), canary(100)].concat(),
        "clamped up"
    );
    assert_eq!(
        weights(-5),
        [stable(100), canary(0)].concat(),
        "clamped down"
    );
}

#[test]
fn canary_labels_drop_the_stable_selector_keys_only() {
    let labels = labels();
    assert_eq!(labels.get("app"), None, "stable selector key must not leak");
    assert_eq!(labels.get("tier"), Some(&json!("backend")), "kept");
    assert_eq!(labels.get(PIPELINE_LABEL), Some(&json!("web")));
    assert_eq!(labels.get(CANARY_LABEL), Some(&json!("true")));
    assert_eq!(labels.get(REVISION_LABEL), Some(&json!("v2")));
    // The selector never carries the revision label (it is immutable).
    let selector = selector_labels("web");
    assert_eq!(
        selector,
        json!({"init-pro.io/pipeline": "web", "init-pro.io/canary": "true"})
    );
    assert_eq!(selector.get(REVISION_LABEL), None);
    // Absent inputs degrade to the identity labels alone.
    let bare = canary_labels("web", &Value::Null, &Value::Null, "rev");
    assert_eq!(bare.len(), 3);
}

#[test]
fn step_decision_soaks_each_step_and_promotes_last() {
    let plan = plan_for("v2");
    let roll = |step: i32, revision: &str, since: &str| api::pipeline::RolloutStatus {
        step,
        weight: 0,
        revision: revision.to_string(),
        since: since.to_string(),
    };
    // Fresh release: step 0.
    assert_eq!(
        step_decision(None, &plan, 1_000),
        StepDecision::Apply { step: 0 }
    );
    // Step 0 soaks 30s from t=10: still waiting at t=20, advances at t=40.
    let at = |step: i32| roll(step, "team/web:v2", "1970-01-01T00:00:10Z");
    assert_eq!(
        step_decision(Some(&at(0)), &plan, 20),
        StepDecision::Wait { step: 0 }
    );
    assert_eq!(
        step_decision(Some(&at(0)), &plan, 40),
        StepDecision::Apply { step: 1 }
    );
    // A different revision restarts at step 0 (never promotes a stale image).
    let stale = roll(2, "team/web:v1", "1970-01-01T00:00:10Z");
    assert_eq!(
        step_decision(Some(&stale), &plan, 40),
        StepDecision::Apply { step: 0 }
    );
    // Garbage `since` counts as "just applied": it soaks, never skips.
    let unparsed = roll(0, "team/web:v2", "not-a-time");
    assert_eq!(
        step_decision(Some(&unparsed), &plan, 40),
        StepDecision::Wait { step: 0 }
    );
    // Negative step restarts; out-of-range positive step promotes.
    let negative = roll(-1, "team/web:v2", "1970-01-01T00:00:10Z");
    assert_eq!(
        step_decision(Some(&negative), &plan, 40),
        StepDecision::Apply { step: 0 }
    );
    let beyond = roll(7, "team/web:v2", "1970-01-01T00:00:10Z");
    assert_eq!(
        step_decision(Some(&beyond), &plan, 40),
        StepDecision::Promote
    );
    // The last step has no pause: it promotes as soon as it is reached.
    let last = roll(2, "team/web:v2", "1970-01-01T00:00:10Z");
    assert_eq!(step_decision(Some(&last), &plan, 40), StepDecision::Promote);
}

#[test]
fn clone_deployment_rewrites_selector_labels_and_image() {
    let plan = plan_for("v2");
    let canary = clone_deployment(&stable_deployment(), &plan, &labels(), owner());
    assert_eq!(canary["apiVersion"], json!("apps/v1"));
    assert_eq!(canary["kind"], json!("Deployment"));
    assert_eq!(canary.pointer("/metadata/name"), Some(&json!("web-canary")));
    assert_eq!(
        canary.pointer("/metadata/namespace"),
        Some(&json!("default"))
    );
    assert_eq!(
        canary.pointer("/metadata/ownerReferences/0"),
        Some(&owner())
    );
    assert_eq!(
        canary.pointer("/metadata/labels"),
        Some(&json!({"init-pro.io/pipeline": "web", "init-pro.io/canary": "true"}))
    );
    // Nothing volatile/stable-only leaks into the child.
    assert_eq!(canary.pointer("/metadata/uid"), None);
    assert_eq!(canary.pointer("/metadata/annotations"), None);
    assert_eq!(canary.get("status"), None);
    // The pod spec is cloned; replicas survive.
    assert_eq!(canary.pointer("/spec/replicas"), Some(&json!(3)));
    assert_eq!(
        canary.pointer("/spec/selector/matchLabels"),
        Some(&selector_labels("web"))
    );
    let template = canary.pointer("/spec/template/metadata/labels").unwrap();
    assert_eq!(template.get("app"), None, "stable selector key removed");
    assert_eq!(template.get(PIPELINE_LABEL), Some(&json!("web")));
    assert_eq!(template.get(REVISION_LABEL), Some(&json!("v2")));
    assert_eq!(
        canary.pointer("/spec/template/spec/containers/0/image"),
        Some(&json!("team/web:v2"))
    );
    // A missing stable replicas field defaults to 1.
    let mut bare = stable_deployment();
    bare["spec"].as_object_mut().unwrap().remove("replicas");
    let canary = clone_deployment(&bare, &plan, &labels(), owner());
    assert_eq!(canary.pointer("/spec/replicas"), Some(&json!(1)));
}

#[test]
fn clone_service_forces_cluster_ip_and_strips_the_node_port() {
    let plan = plan_for("v2");
    let labels = labels();
    let svc = clone_service(&stable_service(), &plan, &labels, owner());
    assert_eq!(svc["apiVersion"], json!("v1"));
    assert_eq!(svc["kind"], json!("Service"));
    assert_eq!(svc.pointer("/metadata/name"), Some(&json!("web-canary")));
    assert_eq!(svc.pointer("/spec/type"), Some(&json!("ClusterIP")));
    assert_eq!(
        svc.pointer("/spec/selector"),
        Some(&Value::Object(labels.clone()))
    );
    // The ports survive minus the node port (the entry Service owns it).
    assert_eq!(svc.pointer("/spec/ports/0/name"), Some(&json!("http")));
    assert_eq!(svc.pointer("/spec/ports/0/port"), Some(&json!(80)));
    assert_eq!(svc.pointer("/spec/ports/0/targetPort"), Some(&json!(8080)));
    assert_eq!(svc.pointer("/spec/ports/0/protocol"), Some(&json!("TCP")));
    assert_eq!(svc.pointer("/spec/ports/0/nodePort"), None);
    assert_eq!(
        svc.pointer("/spec/sessionAffinity"),
        Some(&json!("ClientIP"))
    );
    for absent in [
        "/spec/clusterIP",
        "/spec/clusterIPs",
        "/spec/ipFamilies",
        "/spec/ipFamilyPolicy",
        "/spec/healthCheckNodePort",
        "/spec/externalTrafficPolicy",
    ] {
        assert_eq!(svc.pointer(absent), None, "{absent} must not be copied");
    }
}

#[test]
fn merges_converge_and_are_idempotent() {
    let plan = plan_for("v2");
    let labels = labels();
    let desired = clone_deployment(&stable_deployment(), &plan, &labels, owner());
    let first = merge_deployment(&stable_deployment(), &desired).expect("first pass changes");
    assert_eq!(
        first.pointer("/spec/template/spec/containers/0/image"),
        Some(&json!("team/web:v2")),
        "only the image is rewritten, the container name survives"
    );
    assert_eq!(
        first.get("status"),
        stable_deployment().get("status"),
        "non-owned fields are preserved on merge"
    );
    assert_eq!(
        merge_deployment(&first, &desired),
        None,
        "second pass no-ops"
    );

    let desired = clone_service(&stable_service(), &plan, &labels, owner());
    let first = merge_service(&stable_service(), &desired).expect("first pass changes");
    assert_eq!(first.pointer("/spec/type"), Some(&json!("ClusterIP")));
    assert_eq!(first.pointer("/spec/clusterIP"), Some(&json!("10.43.1.2")));
    assert_eq!(merge_service(&first, &desired), None, "second pass no-ops");
}

#[test]
fn releases_default_canary_names_and_tolerate_abort_shapes() {
    // No canaryService/canaryDeployment: the defaults are derived.
    let sp = pipeline("v2");
    let release = parse_release(&sp).expect("release");
    let plan = validate_names(&sp, &release).expect("names validate");
    assert_eq!(plan.canary_service, "web-canary");
    assert_eq!(plan.canary_deployment, "web-canary");
    assert_eq!(plan.revision, "team/web:v2");
    assert_eq!(plan.revision_label, "v2");
    // The abort path needs neither steps nor a target Deployment.
    let mut bare = pipeline("v2");
    bare["spec"]["target"] = Value::Null;
    bare["spec"]["release"] = json!({"trafficSplit": "web", "stableService": "web"});
    let release = parse_release(&bare).expect("release");
    let plan = validate_names(&bare, &release).expect("names-only path");
    assert_eq!(
        plan.canary_deployment, "web-canary",
        "defaulted from stable"
    );
    assert_eq!(plan.stable_deployment, "");
}

#[test]
fn name_validation_names_the_offending_field() {
    let cases: [(&str, Value, &str); 4] = [
        ("trafficSplit", json!(""), "spec.release.trafficSplit"),
        ("trafficSplit", json!("Web"), "spec.release.trafficSplit"),
        ("trafficSplit", json!("-web"), "spec.release.trafficSplit"),
        (
            "trafficSplit",
            json!("web".repeat(22)), // 66 chars
            "spec.release.trafficSplit",
        ),
    ];
    for (field, value, fragment) in cases {
        let mut sp = pipeline("v2");
        sp["spec"]["release"][field] = value;
        let error = validate_err(&sp);
        assert!(error.contains(fragment), "expected {fragment} in {error}");
    }
    let mut sp = pipeline("v2");
    sp["spec"]["release"]["stableService"] = json!("-web");
    assert!(validate_err(&sp).contains("spec.release.stableService"));
    // The canary children may never shadow the stable pair.
    let mut sp = pipeline("v2");
    sp["spec"]["release"]["canaryService"] = json!("web");
    assert!(validate_err(&sp).contains("canaryService must differ"));
    let mut sp = pipeline("v2");
    sp["spec"]["release"]["canaryDeployment"] = json!("web");
    assert!(validate_err(&sp).contains("canaryDeployment must differ"));
    // Step/target requirements of the roll path.
    let mut sp = pipeline("v2");
    sp["spec"]["release"]["steps"] = json!([]);
    assert!(validate_err(&sp).contains("spec.release.steps"));
    let mut sp = pipeline("v2");
    sp["spec"]["release"]["steps"] = json!([{"weight": 101}]);
    assert!(validate_err(&sp).contains("spec.release.steps[0].weight"));
    let mut sp = pipeline("v2");
    sp["spec"]["target"] = json!({"deployment": "", "container": "app"});
    assert!(validate_err(&sp).contains("spec.target.deployment"));
}

#[test]
fn ownership_requires_the_controlling_pipeline() {
    let owned = json!({"metadata": {"ownerReferences": [owner()]}});
    assert!(owned_by(&owned, "web", "uid-web"));
    assert!(owned_by(&owned, "web", ""), "blank uid matches");
    assert!(!owned_by(&owned, "web", "other-uid"), "uid mismatch");
    assert!(!owned_by(&owned, "api", "uid-web"), "name mismatch");
    let foreign = json!({"metadata": {"name": "web-canary"}});
    assert!(!owned_by(&foreign, "web", "uid-web"));
    let not_controller = json!({"metadata": {"ownerReferences": [
        {"kind": "ServicePipeline", "name": "web", "controller": false}
    ]}});
    assert!(!owned_by(&not_controller, "web", "uid-web"));
}
