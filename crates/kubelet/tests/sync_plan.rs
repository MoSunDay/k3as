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
            ccfg: expect_ccfg(&pod, 0),
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
                ccfg: expect_ccfg(&pod, 1),
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
        }],
        ..Default::default()
    };
    let actions = plan(&BTreeMap::new(), &snap, SANDBOX_IMAGE, Path::new(LOG_ROOT));
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
        }],
        ..Default::default()
    };
    let actions = plan(&BTreeMap::new(), &snap, SANDBOX_IMAGE, Path::new(LOG_ROOT));
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
        }],
    };
    let actions = plan_one(&pod, &snap);
    assert!(matches!(actions[0], Action::RemoveContainer { ref id, .. } if id == "c1"));
    assert!(matches!(actions[1], Action::CreateStartContainer { .. }));
}
