//! Host-direct admission rules (T4.15/T7.2, Q60/Q61).
//!
//! Split out of `validate` to respect the file-size limit; the entry
//! point stays `validate::admit`, which delegates here. Since the S47
//! review F2 move this module owns the GPU/host-direct rules for every
//! pod shape:
//!
//! 1. **shared** (`validate_gpu_requires_host_direct`, called from
//!    `validate::validate_pod`): any GPU-requesting owner — a bare Pod or
//!    an apps/v1 workload template — must carry the literal
//!    `init-pro.io/rootfs: "/"` annotation (whole-GPU pods are
//!    host-direct-only, Q61). The rule used to live in the
//!    Deployment-only branch.
//! 2. **Deployment-only** (`validate_deployment`): a literal
//!    `init-pro.io/rootfs: "/"` template must deploy with
//!    `strategy.type: Recreate` (the default RollingUpdate is rejected).

use serde_json::Value;

use crate::validate::{gpu_entries, HOST_DIRECT_ROOT, ROOTFS_ANNOTATION};

/// Shared host-direct rule (Q61; S47 review F2): any GPU resource
/// requested by the owner's containers (requests or limits) requires the
/// literal `init-pro.io/rootfs: "/"` annotation. `path` is the owner's
/// spec path (`"spec"` for Pods, `"spec.template.spec"` for workload
/// templates) and drives the annotation path in the message.
pub(crate) fn validate_gpu_requires_host_direct(owner: &Value, path: &str) -> Result<(), String> {
    let gpus = pod_gpu_requests(owner);
    if gpus.is_empty() {
        return Ok(());
    }
    // Annotations live beside the spec: "spec" -> "metadata.annotations",
    // "spec.template.spec" -> "spec.template.metadata.annotations".
    let annotations_path = match path.strip_suffix(".spec") {
        Some(owner_prefix) => format!("{owner_prefix}.metadata.annotations"),
        None => "metadata.annotations".to_string(),
    };
    if !is_host_direct(owner) {
        return Err(format!(
            "{annotations_path}: Required value: GPU requests ({gpus}) require host-direct: set \
             the annotation {ROOTFS_ANNOTATION}: \"{HOST_DIRECT_ROOT}\" (whole-GPU pods are \
             host-direct-only, Q61)"
        ));
    }
    Ok(())
}

/// Deployment-only host-direct rule (Q60): a literal
/// `init-pro.io/rootfs: "/"` template must deploy with
/// `strategy.type: Recreate` (the default RollingUpdate is rejected).
/// (The GPU-requires-annotation rule moved up to the shared pod path,
/// `validate_gpu_requires_host_direct`, S47 review F2.)
pub(crate) fn validate_deployment(dep: &Value) -> Result<(), String> {
    let host_direct = dep.pointer("/spec/template").is_some_and(is_host_direct);
    if !host_direct {
        return Ok(());
    }
    let effective = dep
        .pointer("/spec/strategy/type")
        .and_then(Value::as_str)
        .unwrap_or("RollingUpdate"); // upstream default
    if effective != "Recreate" {
        return Err(format!(
            "spec.strategy.type: Invalid value: \"{effective}\": host-direct template \
             (annotation {ROOTFS_ANNOTATION}: \"{HOST_DIRECT_ROOT}\") requires strategy.type \
             Recreate: host-direct pods must not roll alongside their predecessors"
        ));
    }
    Ok(())
}

/// True when the owner's rootfs annotation is the literal `/` (Q60).
/// Owner-generic since the S47 review F2 move: it reads a bare Pod or a
/// workload template. (Manual map lookup: the annotation key contains
/// `/`, which a JSON pointer cannot address.)
fn is_host_direct(owner: &Value) -> bool {
    owner
        .pointer("/metadata/annotations")
        .and_then(Value::as_object)
        .and_then(|m| m.get(ROOTFS_ANNOTATION))
        .and_then(Value::as_str)
        .is_some_and(|v| v == HOST_DIRECT_ROOT)
}

/// Comma-joined GPU resource names an owner's containers request (both
/// `requests` and `limits`: upstream defaults requests from limits, so a
/// limits-only GPU container is a GPU-requesting container). Empty = none.
fn pod_gpu_requests(owner: &Value) -> String {
    let mut names: Vec<&str> = Vec::new();
    for list in ["containers", "initContainers"] {
        for container in owner
            .pointer(&format!("/spec/{list}"))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            for (res, _, _) in gpu_entries(container, "requests")
                .into_iter()
                .chain(gpu_entries(container, "limits"))
            {
                if !names.contains(&res) {
                    names.push(res);
                }
            }
        }
    }
    names.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::validate::admit;
    use serde_json::json;

    fn container(resources: Value) -> Value {
        let mut c = json!({"name": "c", "image": "i"});
        if !resources.is_null() {
            c["resources"] = resources;
        }
        c
    }

    fn deployment(strategy: Option<&str>, annotations: Value, c: Value) -> Value {
        let mut spec = json!({"template": {"metadata": {"annotations": annotations},
                                           "spec": {"containers": [c]}}});
        if let Some(s) = strategy {
            spec["strategy"] = json!({"type": s});
        }
        json!({"apiVersion": "apps/v1", "kind": "Deployment", "spec": spec})
    }

    fn admit_dep(strategy: Option<&str>, annotations: Value, c: Value) -> Result<(), String> {
        admit("apps", "Deployment", &deployment(strategy, annotations, c))
    }

    // --- Deployment host-direct rules (2c) ------------------------------------
    #[test]
    fn host_direct_template_requires_recreate_strategy() {
        let rootfs = json!({ROOTFS_ANNOTATION: "/"});
        // Absent strategy (upstream default RollingUpdate) and explicit
        // RollingUpdate are both rejected, mentioning host-direct/Recreate.
        for dep in [
            deployment(None, rootfs.clone(), container(Value::Null)),
            deployment(
                Some("RollingUpdate"),
                rootfs.clone(),
                container(Value::Null),
            ),
        ] {
            let err = admit("apps", "Deployment", &dep).unwrap_err();
            assert!(
                err.contains("spec.strategy.type: Invalid value: \"RollingUpdate\"")
                    && err.contains("host-direct")
                    && err.contains("Recreate"),
                "{err}"
            );
        }
        // Recreate (with or without an integral GPU request) is accepted.
        for res in [Value::Null, json!({"limits": {"nvidia.com/gpu": "1"}})] {
            assert!(admit_dep(Some("Recreate"), rootfs.clone(), container(res)).is_ok());
        }
    }

    #[test]
    fn gpu_template_requires_the_literal_rootfs_annotation() {
        let err = admit_dep(
            None,
            json!({}),
            container(json!({"requests": {"nvidia.com/gpu": "1"}})),
        )
        .unwrap_err();
        assert!(
            err.contains("spec.template.metadata.annotations: Required value:")
                && err.contains("nvidia.com/gpu")
                && err.contains("host-direct")
                && err.contains("init-pro.io/rootfs"),
            "{err}"
        );
        // A non-literal value does not satisfy the rule either.
        let err = admit_dep(
            None,
            json!({ROOTFS_ANNOTATION: "host"}),
            container(json!({"limits": {"amd.com/gpu": 1}})),
        )
        .unwrap_err();
        assert!(err.contains("amd.com/gpu"), "{err}");
    }

    // --- shared rule covers bare Pods + every template (S47 review F2) --------

    #[test]
    fn bare_pod_gpu_requires_the_literal_rootfs_annotation() {
        // Bare Pods are rejected with the pod-level annotation path; with
        // the literal annotation the same GPU pod is admitted.
        let mut pod = json!({"apiVersion": "v1", "kind": "Pod", "metadata": {"name": "p"},
            "spec": {"containers": [container(json!({"requests": {"nvidia.com/gpu": "1"}}))]}});
        let err = admit("", "Pod", &pod).unwrap_err();
        assert!(
            err.contains("metadata.annotations: Required value:")
                && err.contains("GPU requests (nvidia.com/gpu) require host-direct")
                && err.contains("init-pro.io/rootfs"),
            "{err}"
        );
        pod["metadata"]["annotations"] = json!({ROOTFS_ANNOTATION: "/"});
        assert!(admit("", "Pod", &pod).is_ok());
    }

    #[test]
    fn gpu_rule_covers_rs_sts_and_daemonset_templates() {
        // Kind dispatch: every apps/v1 template kind hits the shared rule
        // (it is no longer Deployment-only); an annotated template passes.
        for kind in ["ReplicaSet", "StatefulSet", "DaemonSet"] {
            let mut body = json!({"apiVersion": "apps/v1", "kind": kind,
                "spec": {"template": {"metadata": {"labels": {"app": "x"}},
                    "spec": {"containers": [container(json!({"limits": {"amd.com/gpu": 1}}))]}}}});
            let err = admit("apps", kind, &body).unwrap_err();
            assert!(
                err.contains("spec.template.metadata.annotations: Required value:")
                    && err.contains("amd.com/gpu"),
                "{kind}: {err}"
            );
            body["spec"]["template"]["metadata"]["annotations"] = json!({ROOTFS_ANNOTATION: "/"});
            assert!(admit("apps", kind, &body).is_ok(), "{kind} annotated");
        }
    }
}
