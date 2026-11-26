//! Pure-decision tests for the pipeline controller (TODO **T8.3**, Q56).
//!
//! Child module of [`super`] (`plan`), so every helper is reachable through
//! the `super::*` glob without widening its visibility.

use std::time::Duration;

use super::*;

fn context_build() -> Value {
    json!({
        "spec": {
            "image": {"repository": "reg:5000/team/app", "tag": "b1"},
            "source": {"context": "/src/app/", "dockerfile": "Dockerfile.prod"},
            "args": ["A=1", "", "B=2"],
            "timeoutSeconds": 42,
        }
    })
}

#[test]
fn join_logs_and_blank_detection() {
    assert_eq!(join_logs("", "b"), "b");
    assert_eq!(join_logs("a", ""), "a");
    assert_eq!(join_logs("a", "b"), "a\nb");
    assert!(blank(None));
    assert!(blank(Some(&json!(""))));
    assert!(!blank(Some(&json!("x"))));
}

#[test]
fn timeout_prefers_the_spec_and_falls_back_to_the_config() {
    assert_eq!(timeout_for(&context_build(), 900), Duration::from_secs(42));
    assert_eq!(timeout_for(&json!({}), 900), Duration::from_secs(900));
    assert_eq!(
        timeout_for(&json!({"spec": {"timeoutSeconds": 0}}), 900),
        Duration::from_secs(900)
    );
}

#[test]
fn dockerfile_argument_defaults_and_stays_absolute_when_given() {
    let mut build = context_build();
    build["spec"]["source"]["dockerfile"] = json!("");
    assert_eq!(dockerfile_arg(&build), None);
    build["spec"]["source"]["dockerfile"] = json!("docker/Dev.Dockerfile");
    assert_eq!(
        dockerfile_arg(&build),
        Some("/src/app/docker/Dev.Dockerfile".into())
    );
    build["spec"]["source"]["dockerfile"] = json!("/abs/Dockerfile");
    assert_eq!(dockerfile_arg(&build), Some("/abs/Dockerfile".into()));
}

#[test]
fn build_args_render_the_docker_build_invocation() {
    let args = build_args(&context_build(), "reg:5000/team/app:b1");
    assert_eq!(
        args,
        vec![
            "build",
            "-f",
            "/src/app/Dockerfile.prod",
            "-t",
            "reg:5000/team/app:b1",
            "--build-arg",
            "A=1",
            "--build-arg",
            "B=2",
            "/src/app/",
        ]
    );
}

#[test]
fn build_names_are_dns_safe_and_bounded() {
    assert_eq!(
        build_name_with("code-grapher", "Build_42/v2", "ab12c"),
        "code-grapher-build-42-v2-ab12c"
    );
    // Slugs collapse runs, trim ends, and fall back on empty input.
    assert_eq!(build_name_with("a__b", "", "x"), "a-b-build-x");
    let long = "p".repeat(80);
    assert_eq!(
        build_name_with(&long, "t", "x"),
        format!("{}-t-x", "p".repeat(40))
    );
    assert!(build_name_with("", "", "x").starts_with("pipeline-build-"));
}

#[test]
fn container_image_patch_selects_by_name_then_first() {
    let mut dep = json!({"spec": {"template": {"spec": {"containers": [
        {"name": "app", "image": "old:1"},
        {"name": "sidecar", "image": "side:1"},
    ]}}}});
    assert!(set_container_image(&mut dep, "sidecar", "new:2"));
    assert_eq!(
        dep["spec"]["template"]["spec"]["containers"][1]["image"],
        json!("new:2")
    );
    assert_eq!(
        dep["spec"]["template"]["spec"]["containers"][0]["image"],
        json!("old:1")
    );
    // Empty container name picks the first container.
    assert!(set_container_image(&mut dep, "", "first:3"));
    assert_eq!(
        dep["spec"]["template"]["spec"]["containers"][0]["image"],
        json!("first:3")
    );
    // Unknown container / missing template are reported, not panicked.
    assert!(!set_container_image(&mut dep, "ghost", "x:1"));
    assert!(!set_container_image(&mut json!({"spec": {}}), "", "x:1"));
}

#[test]
fn owned_build_needs_the_pipeline_controller_owner() {
    let owner = |kind: &str, ns_name: &str, uid: &str| {
        json!({"metadata": {"ownerReferences": [
            {"kind": kind, "name": ns_name, "uid": uid, "controller": true},
        ]}})
    };
    assert!(owned_build(&owner("ServicePipeline", "p", "u1"), "p", "u1"));
    // uid only compared when BOTH sides carry one.
    assert!(owned_build(&owner("ServicePipeline", "p", ""), "p", "u1"));
    assert!(owned_build(&owner("ServicePipeline", "p", "u1"), "p", ""));
    assert!(!owned_build(
        &owner("ServicePipeline", "p", "u2"),
        "p",
        "u1"
    ));
    // Foreign owner kinds / names / non-controlled objects never match.
    assert!(!owned_build(&owner("Build", "p", "u1"), "p", "u1"));
    assert!(!owned_build(
        &owner("ServicePipeline", "other", "u1"),
        "p",
        "u1"
    ));
    assert!(!owned_build(&json!({"metadata": {}}), "p", "u1"));
}

#[test]
fn latest_build_orders_by_creation_timestamp_then_name() {
    let a = json!({"metadata": {"name": "a", "creationTimestamp": "2026-01-01T00:00:00Z"}});
    let b = json!({"metadata": {"name": "b", "creationTimestamp": "2026-01-02T00:00:00Z"}});
    let c = json!({"metadata": {"name": "c"}});
    let owned = vec![&a, &b, &c];
    assert_eq!(name(latest_build(&owned).unwrap()), Some("b"));
    // Without timestamps the name breaks the tie.
    let d = json!({"metadata": {"name": "d"}});
    let owned = vec![&c, &d];
    assert_eq!(name(latest_build(&owned).unwrap()), Some("d"));
    assert!(latest_build(&[]).is_none());
}

#[test]
fn latest_build_for_tag_ignores_other_tags_regardless_of_name_order() {
    // Batch F soak finding: without creationTimestamp `p-b9-x` sorts above
    // `p-b10-x`; the desired tag must still resolve to its own child.
    let b9 = json!({"metadata": {"name": "p-b9-x"}, "spec": {"image": {"tag": "b9"}}});
    let b10 = json!({"metadata": {"name": "p-b10-x"}, "spec": {"image": {"tag": "b10"}}});
    let b10_dup = json!({"metadata": {"name": "p-b10-y"}, "spec": {"image": {"tag": "b10"}}});
    let owned = vec![&b9, &b10, &b10_dup];
    assert_eq!(name(latest_build(&owned).unwrap()), Some("p-b9-x")); // the trap
    assert_eq!(
        name(latest_build_for_tag(&owned, "b10").unwrap()),
        Some("p-b10-y")
    );
    assert_eq!(
        name(latest_build_for_tag(&owned, "b9").unwrap()),
        Some("p-b9-x")
    );
    assert!(latest_build_for_tag(&owned, "b11").is_none());
}

#[test]
fn phases_mirror_and_terminate() {
    let publishing = json!({"status": {"phase": "Publishing"}});
    let building = json!({"status": {"phase": "Pending"}});
    assert_eq!(active_phase(&publishing), api::pipeline::PHASE_PUBLISHING);
    assert_eq!(active_phase(&building), api::pipeline::PHASE_BUILDING);
    assert!(!is_terminal(&publishing));
    assert!(!is_terminal(&json!({})));
    assert!(is_terminal(&json!({"status": {"phase": "Succeeded"}})));
    assert!(is_terminal(&json!({"status": {"phase": "Failed"}})));
}

// ===================== host image hygiene (B8) =====================
// Pure argv + retention selection: no docker, no storage.

fn labeled_build() -> Value {
    let mut build = context_build();
    build["metadata"] = json!({"name": "app-b1-ab12c", "namespace": "team"});
    build["spec"]["pipeline"] = json!("app");
    build
}

#[test]
fn build_args_label_the_image_with_the_owning_pipeline() {
    let args = build_args(&labeled_build(), "reg:5000/team/app:b1");
    let label = args
        .iter()
        .position(|a| a == "--label")
        .expect("the build carries a --label");
    assert_eq!(args[label + 1], "init-pro.pipeline=team/app");
    // The label rides along the rest of the invocation, never replacing it.
    assert_eq!(args[0], "build");
    assert!(args.contains(&"-t".to_string()));
    assert_eq!(args.last().unwrap(), "/src/app/");
}

#[test]
fn build_args_omit_the_label_without_a_pipeline_identity() {
    // No metadata / spec.pipeline: nothing to select images by later.
    let args = build_args(&context_build(), "reg:5000/team/app:b1");
    assert!(!args.contains(&"--label".to_string()));
    assert_eq!(image_label_value(&context_build()), None);
    // A namespace without a pipeline (or the reverse) is still unusable.
    let mut half = context_build();
    half["metadata"] = json!({"namespace": "team"});
    assert_eq!(image_label_value(&half), None);
    let mut other = context_build();
    other["spec"]["pipeline"] = json!("app");
    assert_eq!(image_label_value(&other), None);
}

#[test]
fn list_and_rmi_argv_render_the_docker_calls() {
    assert_eq!(
        list_images_args("team/app"),
        vec![
            "images",
            "--filter",
            "label=init-pro.pipeline=team/app",
            "--format",
            "{{.ID}}\t{{.Repository}}:{{.Tag}}",
        ]
    );
    assert_eq!(
        rmi_args("reg:5000/team/app:b1"),
        vec!["rmi", "reg:5000/team/app:b1"]
    );
    assert_eq!(BUILD_IMAGE_RETENTION, 3);
}

#[test]
fn prune_targets_keep_the_newest_n_and_skip_log_noise() {
    let listing = "$ docker images --filter label=init-pro.pipeline=team/app --format ...\n\
                   abc123\treg:5000/team/app:b4\n\
                   def456\treg:5000/team/app:b3\n\
                   789abc\treg:5000/team/app:b2\n\
                   0a0b0c\treg:5000/team/app:b1\n\
                   fff000\t<none>:<none>\n";
    // Docker lists newest first: the three newest stay, the rest goes.
    assert_eq!(
        prune_targets(listing, 3),
        vec!["reg:5000/team/app:b1".to_string(), "fff000".to_string()],
        "the log header is skipped and an untagged row falls back to its ID"
    );
    // Within the retention window nothing is removed.
    assert!(prune_targets(listing, 5).is_empty());
    assert!(prune_targets("", 3).is_empty());
    // A daemon warning line is ignored rather than pruned.
    assert!(prune_targets("warning: something odd\n", 0).is_empty());
}

#[test]
fn promoted_image_detection_keeps_the_running_copy() {
    assert!(is_promoted("reg:5000/team/app:b1", "reg:5000/team/app:b1"));
    // A different image, or nothing promoted yet, is prunable.
    assert!(!is_promoted("reg:5000/team/app:b2", "reg:5000/team/app:b1"));
    assert!(!is_promoted("reg:5000/team/app:b2", ""));
}

#[test]
fn push_retry_counter_caps_at_max() {
    assert_eq!(next_push_retry(&json!({})), Some(1));
    assert_eq!(next_push_retry(&json!({"pushRetries": 0})), Some(1));
    assert_eq!(next_push_retry(&json!({"pushRetries": 4})), Some(5));
    assert_eq!(next_push_retry(&json!({"pushRetries": 5})), None);
    assert_eq!(next_push_retry(&json!({"pushRetries": 9})), None);
    // malformed counters are read as zero, never as a free pass
    assert_eq!(next_push_retry(&json!({"pushRetries": "x"})), Some(1));
    assert_eq!(next_push_retry(&json!({"pushRetries": -1})), Some(1));
}
