//! Unit tests for the pure reconcile core `kubelet::sync::plan` (TODO
//! **T4.2**): desired pod set vs CRI snapshot -> ordered action list. The
//! kill->restart path (exited container recreated with attempt+1), teardown
//! ordering, cross-namespace disambiguation, and steady-state idempotence
//! are all pinned here without any containerd or apiserver involved.

mod support;

use std::collections::BTreeMap;
use std::path::Path;

use kubelet::cri_backend::{ContainerView, SandboxView};
use kubelet::{plan, Action, Snapshot};
use runtime::cri_json::container_labels;
use support::*;

const LOG_ROOT: &str = "/tmp/init-pro-kubelet-test/pods";

#[test]
fn fresh_pod_ensures_images_then_creates_sandbox() {
    let pod = view("default", "web", "u1", &["img:1"]);
    let actions = plan_one(&pod, &Snapshot::default());
    assert_eq!(
        actions,
        vec![
            Action::EnsureImage(SANDBOX_IMAGE.into()),
            Action::EnsureImage("img:1".into()),
            Action::CreateSandbox {
                pod_key: "default/web".into(),
                cfg: expect_pcfg(LOG_ROOT)
            },
        ]
    );
}

#[test]
fn present_images_are_not_pulled() {
    let pod = view("default", "web", "u1", &["img:1"]);
    let snap = Snapshot {
        images: vec![SANDBOX_IMAGE.into(), "img:1".into()],
        ..Default::default()
    };
    let actions = plan_one(&pod, &snap);
    assert_eq!(actions.len(), 1);
    assert!(matches!(actions[0], Action::CreateSandbox { .. }));
}

#[test]
fn ready_sandbox_without_containers_creates_and_starts_them() {
    let pod = view("default", "web", "u1", &["img:1"]);
    let snap = Snapshot {
        images: vec![SANDBOX_IMAGE.into(), "img:1".into()],
        sandboxes: vec![sandbox("default", "web", "u1", "sb1", "SANDBOX_READY")],
        containers: vec![],
    };
    let actions = plan_one(&pod, &snap);
    assert_eq!(
        actions,
        vec![Action::CreateStartContainer {
            pod_key: "default/web".into(),
            sandbox_id: "sb1".into(),
            ccfg: Box::new(expect_ccfg(&pod, 0)),
            pcfg: expect_pcfg(LOG_ROOT),
        }]
    );
}

#[test]
fn exited_container_is_recreated_with_attempt_plus_one() {
    // THE kill->restart case: the container died (attempt 0), so plan must
    // remove it and recreate with attempt 1 and a matching log_path.
    let pod = view("default", "web", "u1", &["img:1"]);
    let snap = Snapshot {
        images: vec![SANDBOX_IMAGE.into(), "img:1".into()],
        sandboxes: vec![sandbox("default", "web", "u1", "sb1", "SANDBOX_READY")],
        containers: vec![container(&pod, "c0", "c-old", "sb1", "CONTAINER_EXITED", 0)],
    };
    let actions = plan_one(&pod, &snap);
    assert_eq!(
        actions,
        vec![
            Action::RemoveContainer {
                pod_key: "default/web".into(),
                id: "c-old".into()
            },
            Action::CreateStartContainer {
                pod_key: "default/web".into(),
                sandbox_id: "sb1".into(),
                ccfg: Box::new(expect_ccfg(&pod, 1)),
                pcfg: expect_pcfg(LOG_ROOT),
            },
        ]
    );
}

#[test]
fn unknown_state_container_also_recreated() {
    let pod = view("default", "web", "u1", &["img:1"]);
    let snap = Snapshot {
        images: vec![SANDBOX_IMAGE.into(), "img:1".into()],
        sandboxes: vec![sandbox("default", "web", "u1", "sb1", "SANDBOX_READY")],
        containers: vec![container(&pod, "c0", "c-x", "sb1", "CONTAINER_UNKNOWN", 2)],
    };
    let actions = plan_one(&pod, &snap);
    assert!(matches!(actions[0], Action::RemoveContainer { ref id, .. } if id == "c-x"));
    let Action::CreateStartContainer { ccfg, .. } = &actions[1] else {
        unreachable!("got {actions:?}")
    };
    assert_eq!(
        ccfg.metadata.attempt, 3,
        "attempt bumps past the last observed"
    );
}

#[test]
fn running_container_steady_state_is_a_noop() {
    let pod = view("default", "web", "u1", &["img:1"]);
    let snap = Snapshot {
        images: vec![SANDBOX_IMAGE.into(), "img:1".into()],
        sandboxes: vec![sandbox("default", "web", "u1", "sb1", "SANDBOX_READY")],
        containers: vec![container(&pod, "c0", "c1", "sb1", "CONTAINER_RUNNING", 0)],
    };
    let actions = plan_one(&pod, &snap);
    assert!(
        actions.is_empty(),
        "steady state must be zero actions: {actions:?}"
    );
}

#[test]
fn deleted_pod_tears_containers_down_before_sandbox() {
    let mut pod = view("default", "web", "u1", &["img:1"]);
    pod.deleted = true;
    let snap = Snapshot {
        sandboxes: vec![sandbox("default", "web", "u1", "sb1", "SANDBOX_READY")],
        containers: vec![container(&pod, "c0", "c1", "sb1", "CONTAINER_RUNNING", 0)],
        ..Default::default()
    };
    let actions = plan_one(&pod, &snap);
    assert_eq!(
        actions,
        vec![
            Action::RemoveContainer {
                pod_key: "default/web".into(),
                id: "c1".into()
            },
            Action::StopRemoveSandbox {
                pod_key: "default/web".into(),
                id: "sb1".into()
            },
        ]
    );
}

#[test]
fn absent_pod_observed_sandbox_is_orphan_teardown() {
    let snap = Snapshot {
        sandboxes: vec![sandbox("default", "gone", "u9", "sb9", "SANDBOX_READY")],
        containers: vec![ContainerView {
            id: "c9".into(),
            sandbox_id: "sb9".into(),
            state: "CONTAINER_RUNNING".into(),
            name: "c0".into(),
            attempt: 0,
            labels: container_labels("gone", "default", "u9", "c0"),
            exit_code: 0,
        }],
        ..Default::default()
    };
    let actions = plan(
        &BTreeMap::new(),
        &snap,
        SANDBOX_IMAGE,
        Path::new(LOG_ROOT),
        &kubelet::PodProbeState::new(),
    );
    assert_eq!(
        actions,
        vec![
            Action::RemoveContainer {
                pod_key: "default/gone".into(),
                id: "c9".into()
            },
            Action::StopRemoveSandbox {
                pod_key: "default/gone".into(),
                id: "sb9".into()
            },
        ]
    );
}

#[test]
fn garbage_sandbox_without_identity_is_ignored() {
    let snap = Snapshot {
        sandboxes: vec![SandboxView {
            id: "junk".into(),
            state: "SANDBOX_READY".into(),
            labels: Default::default(),
            name: String::new(),
            namespace: String::new(),
            uid: String::new(),
            ip: None,
        }],
        ..Default::default()
    };
    let actions = plan(
        &BTreeMap::new(),
        &snap,
        SANDBOX_IMAGE,
        Path::new(LOG_ROOT),
        &kubelet::PodProbeState::new(),
    );
    assert!(
        actions.is_empty(),
        "foreign/garbage sandboxes are not ours: {actions:?}"
    );
}

#[test]
fn two_pods_same_name_different_namespace_are_distinct() {
    let a = view("ns1", "web", "ua", &["img:1"]);
    let b = view("ns2", "web", "ub", &["img:1"]);
    let snap = Snapshot {
        images: vec![SANDBOX_IMAGE.into(), "img:1".into()],
        sandboxes: vec![sandbox("ns1", "web", "ua", "sbA", "SANDBOX_READY")],
        containers: vec![container(&a, "c0", "cA", "sbA", "CONTAINER_RUNNING", 0)],
    };
    let actions = plan(
        &desired_of(&[a, b]),
        &snap,
        SANDBOX_IMAGE,
        Path::new(LOG_ROOT),
        &kubelet::PodProbeState::new(),
    );
    assert_eq!(
        actions.len(),
        1,
        "ns1/web steady; only ns2/web acts: {actions:?}"
    );
    assert!(
        matches!(actions[0], Action::CreateSandbox { ref pod_key, .. } if pod_key == "ns2/web")
    );
}

#[test]
fn sandbox_not_ready_is_torn_down_and_recreated_next_cycle() {
    let pod = view("default", "web", "u1", &["img:1"]);
    let snap = Snapshot {
        sandboxes: vec![sandbox("default", "web", "u1", "sb1", "SANDBOX_NOTREADY")],
        ..Default::default()
    };
    let actions = plan_one(&pod, &snap);
    // Single step per pod: this cycle only tears down (no CreateSandbox).
    assert_eq!(
        actions,
        vec![Action::StopRemoveSandbox {
            pod_key: "default/web".into(),
            id: "sb1".into()
        }]
    );
}

#[test]
fn stale_sandbox_with_wrong_uid_is_torn_down() {
    let pod = view("default", "web", "u-new", &["img:1"]);
    let old_pod = view("default", "web", "u-old", &["img:1"]);
    let snap = Snapshot {
        sandboxes: vec![sandbox("default", "web", "u-old", "sb1", "SANDBOX_READY")],
        containers: vec![container(
            &old_pod,
            "c0",
            "c1",
            "sb1",
            "CONTAINER_RUNNING",
            0,
        )],
        ..Default::default()
    };
    let actions = plan_one(&pod, &snap);
    assert_eq!(
        actions,
        vec![
            Action::RemoveContainer {
                pod_key: "default/web".into(),
                id: "c1".into()
            },
            Action::StopRemoveSandbox {
                pod_key: "default/web".into(),
                id: "sb1".into()
            },
        ],
        "uid mismatch = recreated pod: old sandbox tears down, recreate next cycle"
    );
}

#[test]
fn orphan_containers_without_sandbox_are_removed_and_sandbox_created() {
    let pod = view("default", "web", "u1", &["img:1"]);
    let snap = Snapshot {
        images: vec![SANDBOX_IMAGE.into(), "img:1".into()],
        sandboxes: vec![],
        containers: vec![container(
            &pod,
            "c0",
            "c-lost",
            "sb-gone",
            "CONTAINER_EXITED",
            0,
        )],
    };
    let actions = plan_one(&pod, &snap);
    assert!(matches!(actions[0], Action::RemoveContainer { ref id, .. } if id == "c-lost"));
    assert!(
        matches!(actions[1], Action::CreateSandbox { .. }),
        "got {actions:?}"
    );
}

#[test]
fn duplicate_ready_sandboxes_keep_the_first() {
    let pod = view("default", "web", "u1", &["img:1"]);
    let snap = Snapshot {
        images: vec![SANDBOX_IMAGE.into(), "img:1".into()],
        sandboxes: vec![
            sandbox("default", "web", "u1", "sb1", "SANDBOX_READY"),
            sandbox("default", "web", "u1", "sb2", "SANDBOX_READY"),
        ],
        containers: vec![container(&pod, "c0", "c1", "sb1", "CONTAINER_RUNNING", 0)],
    };
    let actions = plan_one(&pod, &snap);
    assert_eq!(
        actions,
        vec![Action::StopRemoveSandbox {
            pod_key: "default/web".into(),
            id: "sb2".into()
        }]
    );
}

#[test]
fn container_inherits_pod_identity_from_its_sandbox() {
    // A container with no labels still maps to its pod through the sandbox
    // it lives in (crictl always reports podSandboxId).
    let pod = view("default", "web", "u1", &["img:1"]);
    let snap = Snapshot {
        images: vec![SANDBOX_IMAGE.into(), "img:1".into()],
        sandboxes: vec![sandbox("default", "web", "u1", "sb1", "SANDBOX_READY")],
        containers: vec![ContainerView {
            id: "c1".into(),
            sandbox_id: "sb1".into(),
            state: "CONTAINER_EXITED".into(),
            name: "c0".into(),
            attempt: 0,
            labels: Default::default(),
            exit_code: 0,
        }],
    };
    let actions = plan_one(&pod, &snap);
    assert!(matches!(actions[0], Action::RemoveContainer { ref id, .. } if id == "c1"));
    assert!(matches!(actions[1], Action::CreateStartContainer { .. }));
}

#[test]
fn liveness_failure_restarts_running_container() {
    use kubelet::probes::ProbeKind;
    use kubelet::{HttpGetSpec, PodProbeState, ProbeSpec};
    let pod = view_full(
        "default",
        "web",
        "u1",
        "Always",
        serde_json::json!([{"name": "c0", "image": "img:1", "command": ["/bin/c0"]}]),
    );
    let snap = Snapshot {
        images: vec![SANDBOX_IMAGE.into(), "img:1".into()],
        sandboxes: vec![sandbox("default", "web", "u1", "sb1", "SANDBOX_READY")],
        containers: vec![container(&pod, "c0", "c1", "sb1", "CONTAINER_RUNNING", 0)],
    };
    // Liveness tracker folded to Failure (as the runner's probe pass does).
    let mut probes = PodProbeState::new();
    let spec = ProbeSpec {
        http_get: HttpGetSpec { path: "/".into(), port: 80 },
        initial_delay_seconds: 0,
        period_seconds: 1,
        timeout_seconds: 1,
        failure_threshold: 1,
        success_threshold: 1,
    };
    {
        let t = probes.entry(&pod.key, "c0").tracker(ProbeKind::Liveness);
        t.arm(0, &spec);
        t.observe(false, 1, &spec);
    }
    let actions = plan(
        &desired_of(std::slice::from_ref(&pod)),
        &snap,
        SANDBOX_IMAGE,
        Path::new(LOG_ROOT),
        &probes,
    );
    assert_eq!(
        actions,
        vec![
            Action::RemoveContainer {
                pod_key: "default/web".into(),
                id: "c1".into()
            },
            Action::CreateStartContainer {
                pod_key: "default/web".into(),
                sandbox_id: "sb1".into(),
                ccfg: Box::new(expect_ccfg(&pod, 1)),
                pcfg: expect_pcfg(LOG_ROOT),
            },
        ],
        "liveness Failure restarts a RUNNING container with attempt+1"
    );
}

#[test]
fn never_policy_leaves_exited_containers_in_place() {
    let pod = view_full(
        "default",
        "web",
        "u1",
        "Never",
        serde_json::json!([{"name": "c0", "image": "img:1", "command": ["/bin/c0"]}]),
    );
    let snap = Snapshot {
        images: vec![SANDBOX_IMAGE.into(), "img:1".into()],
        sandboxes: vec![sandbox("default", "web", "u1", "sb1", "SANDBOX_READY")],
        containers: vec![container(&pod, "c0", "c1", "sb1", "CONTAINER_EXITED", 0)],
    };
    let actions = plan_one(&pod, &snap);
    assert!(
        actions.is_empty(),
        "Never policy keeps dead containers as-is: {actions:?}"
    );
}

#[test]
fn on_failure_restarts_after_nonzero_exit_only() {
    let pod = view_full(
        "default",
        "web",
        "u1",
        "OnFailure",
        serde_json::json!([{"name": "c0", "image": "img:1", "command": ["/bin/c0"]}]),
    );
    let snap = Snapshot {
        images: vec![SANDBOX_IMAGE.into(), "img:1".into()],
        sandboxes: vec![sandbox("default", "web", "u1", "sb1", "SANDBOX_READY")],
        containers: vec![ContainerView {
            exit_code: 7,
            ..container(&pod, "c0", "c1", "sb1", "CONTAINER_EXITED", 0)
        }],
    };
    let actions = plan_one(&pod, &snap);
    assert!(
        matches!(actions[0], Action::RemoveContainer { .. }),
        "non-zero exit restarts: {actions:?}"
    );
    assert!(
        matches!(actions[1], Action::CreateStartContainer { ref ccfg, .. } if ccfg.metadata.attempt == 1)
    );
    // Exit code 0 -> Completed, no restart.
    let completed = Snapshot {
        images: vec![SANDBOX_IMAGE.into(), "img:1".into()],
        sandboxes: vec![sandbox("default", "web", "u1", "sb1", "SANDBOX_READY")],
        containers: vec![container(&pod, "c0", "c1", "sb1", "CONTAINER_EXITED", 0)],
    };
    assert!(
        plan_one(&pod, &completed).is_empty(),
        "OnFailure keeps exited-0 containers: {:?}",
        plan_one(&pod, &completed)
    );
}

#[test]
fn volume_mounts_are_resolved_into_ccfg() {
    // Sprint 20 S2: plan() is pure, so the CRI mounts are fully asserted
    // here without any IO: emptyDir mounts the volume dir with the declared
    // readOnly; configMap mounts the volume dir read-only; unknown volume
    // names are skipped (the CRI create would fail and the cycle retries).
    let pod = view_with_volumes(
        "default",
        "web",
        "u1",
        &["img:1"],
        serde_json::json!([
            {"name": "cache", "emptyDir": {}},
            {"name": "cfg", "configMap": {"name": "cm1"}},
        ]),
        serde_json::json!([
            {"name": "cache", "mountPath": "/data"},
            {"name": "cfg", "mountPath": "/etc/cm", "readOnly": true},
            {"name": "unknown", "mountPath": "/x"},
        ]),
    );
    let snap = Snapshot {
        images: vec![SANDBOX_IMAGE.into(), "img:1".into()],
        sandboxes: vec![sandbox("default", "web", "u1", "sb1", "SANDBOX_READY")],
        containers: vec![],
    };
    let actions = plan_one(&pod, &snap);
    let Action::CreateStartContainer { ccfg, .. } = &actions[0] else {
        unreachable!("got {actions:?}");
    };
    assert_eq!(
        ccfg.mounts,
        vec![
            runtime::cri_json::Mount {
                container_path: "/data".into(),
                host_path: format!("{LOG_ROOT}/u1/volumes/emptyDir/cache"),
                readonly: false,
            },
            runtime::cri_json::Mount {
                container_path: "/etc/cm".into(),
                host_path: format!("{LOG_ROOT}/u1/volumes/configMap/cm1"),
                readonly: true,
            },
        ]
    );
    // Pods without volumes still produce empty mounts (wire-stable).
    let plain = view("default", "web", "u1", &["img:1"]);
    let actions = plan_one(&plain, &snap);
    let Action::CreateStartContainer { ccfg, .. } = &actions[0] else {
        unreachable!("got {actions:?}");
    };
    assert!(ccfg.mounts.is_empty());
}
