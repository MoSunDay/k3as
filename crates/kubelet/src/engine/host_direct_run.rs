//! Host-direct execution glue for the builtin backend (T4.15, Q60).
//!
//! The pure selection contract lives in [`super::host_direct`]; this
//! module is the backend-facing wrapper: the builtin backend IS
//! `--container-engine=builtin` by construction, so only the engine
//! rootfs flag and the pod annotation remain. Every helper is pure —
//! the backend composes them at the two gates (sandbox create, container
//! start) BEFORE any image planning, and converts outcomes into the
//! actionable HD5 error strings the sync layer reports verbatim as pod
//! events (Q60: an invalid selection must fail the pod, never fall back
//! to containerd or to an image rootfs).

use std::collections::BTreeMap;
use std::path::Path;

use super::host_direct::{select_host_direct, HostDirectSelection, HOST_DIRECT_ROOT};
use super::registry::Pending;
use super::rootfs::{RootfsMode, ROOTFS_ANNOTATION};
use super::spec;

/// Evaluate the Q60 triple from inside the builtin backend (the first
/// leg is structurally true here). `Ok(true)` = host-direct execution;
/// `Ok(false)` = the Q55/Q53 path, byte-compatible; `Err` = the literal
/// `/` annotation with a broken triple — the pod must fail with the
/// carried reason and NOTHING may execute.
pub(super) fn selected(
    engine_rootfs: RootfsMode,
    annotations: &BTreeMap<String, String>,
) -> Result<bool, String> {
    let annotation = annotations.get(ROOTFS_ANNOTATION).map(String::as_str);
    match select_host_direct(true, engine_rootfs, annotation) {
        HostDirectSelection::Selected => Ok(true),
        HostDirectSelection::NotSelected => Ok(false),
        HostDirectSelection::Invalid(reason) => Err(invalid(&reason)),
    }
}

/// HD5: the invalid-selection error, prefixed so pod events and agent
/// logs can grep "host-direct" and see the refusal (not a pull error).
pub(super) fn invalid(reason: &str) -> String {
    format!(
        "host-direct refused: {reason}; host-direct (Q60) never falls back to \
         containerd or an image rootfs — fix the node flags or drop the \
         '{ROOTFS_ANNOTATION}: /' annotation"
    )
}

/// HD5: engine/probe/launch failures on the host-direct path carry the
/// no-fallback remedy.
pub(super) fn failure(stage: &str, error: &str) -> String {
    format!(
        "host-direct {stage} failed: {error}; host-direct (Q60) has no \
         containerd fallback — the builtin engine must run as real root with \
         cgroup v2, or drop the '{ROOTFS_ANNOTATION}: /' annotation"
    )
}

/// HD5 helper: wrap a launch-stage error ONLY on the host-direct path —
/// the Q53/Q55 path keeps its byte-compatible strings untouched.
pub(super) fn maybe_failure(host_direct: bool, stage: &str, error: &str) -> String {
    if host_direct {
        failure(stage, error)
    } else {
        error.to_string()
    }
}

/// T4.15 (Q60): the launch plan for a `Selected` container. Pure — no
/// image resolution, no presence check, no pull, no image config merge:
/// the literal host root IS the rootfs, the pod spec (command/env/
/// limits/mounts) is the whole launch truth (an explicit command is
/// required), and `netns: None` keeps the container in the HOST network
/// namespace (the pause sandbox netns is not joined; the engine child
/// skips both NEWNET and setns, Q48/Q60).
pub(super) fn launch_spec(
    id: &str,
    pending: &Pending,
) -> Result<engine_proto::ContainerSpec, String> {
    let launch = spec::merge_launch(&pending.ccfg, None, None)?;
    spec::container_spec(id, Path::new(HOST_DIRECT_ROOT), pending, None, launch, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ann(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn selection_gates_follow_the_pure_contract() {
        // Triple complete -> Selected; absent/host/image -> Q55 path.
        assert!(selected(RootfsMode::Host, &ann(&[(ROOTFS_ANNOTATION, "/")])).unwrap());
        assert!(!selected(RootfsMode::Host, &ann(&[])).unwrap());
        assert!(!selected(RootfsMode::Host, &ann(&[(ROOTFS_ANNOTATION, "host")])).unwrap());
        assert!(!selected(RootfsMode::Image, &ann(&[(ROOTFS_ANNOTATION, "image")])).unwrap());
        // Broken triple -> the actionable refusal (HD5), never a fallback.
        let err = selected(RootfsMode::Image, &ann(&[(ROOTFS_ANNOTATION, "/")])).unwrap_err();
        assert!(err.contains("host-direct refused"), "{err}");
        assert!(err.contains("--builtin-rootfs=host"), "{err}");
        assert!(err.contains(ROOTFS_ANNOTATION), "{err}");
    }

    #[test]
    fn failure_strings_name_the_remedy() {
        let msg = failure("engine boot", "clone3: EPERM");
        assert!(msg.contains("host-direct engine boot failed"), "{msg}");
        assert!(msg.contains("no containerd fallback"), "{msg}");
        assert!(msg.contains(ROOTFS_ANNOTATION), "{msg}");
    }

    #[test]
    fn maybe_failure_wraps_only_the_host_direct_path() {
        // HD5 wrapping is exclusive: the Q53/Q55 path keeps its strings.
        assert_eq!(
            maybe_failure(false, "spawn", "spawn engine: EPERM"),
            "spawn engine: EPERM"
        );
        assert!(
            maybe_failure(true, "spawn", "spawn engine: EPERM")
                .contains("host-direct spawn failed"),
            "must carry the host-direct prefix"
        );
    }

    #[test]
    fn launch_spec_never_touches_the_image_chain() {
        // A BOGUS, unresolvable image reference still plans fine: on the
        // host-direct path the image is declarative wire metadata only
        // (Q60) — no resolve/pull/presence step is even reachable.
        use runtime::cri_json::{ContainerMetadata, ImageSpec, SandboxMetadata};
        use runtime::{ContainerConfig, PodSandboxConfig};

        let pending = Pending {
            ccfg: ContainerConfig {
                metadata: ContainerMetadata {
                    name: "infer".into(),
                    attempt: 0,
                },
                image: ImageSpec {
                    image: "registry.invalid/no-such/image:never".into(),
                },
                command: vec!["/usr/bin/true".into()],
                log_path: "infer.0.log".into(),
                ..Default::default()
            },
            pcfg: PodSandboxConfig {
                metadata: SandboxMetadata {
                    name: "p-hd".into(),
                    namespace: "default".into(),
                    uid: "u".into(),
                    attempt: 0,
                },
                annotations: ann(&[(ROOTFS_ANNOTATION, "/")]),
                ..Default::default()
            },
            sandbox_id: "sb-hd".into(),
        };
        let es = launch_spec("c-hd", &pending).expect("bogus image must not matter");
        assert!(es.host_root, "host_root must be set");
        assert_eq!(es.rootfs, "/");
        assert!(es.netns.is_none(), "stays in the host network namespace");
        assert!(!es.new_net, "no NEWNET on the host-direct path");
        assert!(es.overlay.is_none(), "no overlay on the host root");
        assert_eq!(es.argv, vec!["/usr/bin/true"]);
    }
}
