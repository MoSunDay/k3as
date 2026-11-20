//! Admission validation for Pod specs and Pod templates (T4.15/T7.2,
//! Q60/Q61 — the api-visible half of the Phase 3 host-direct plane).
//!
//! Pure JSON-only (Q10) checks over `serde_json::Value` request bodies,
//! composed exactly like the create/apply/replace seams in the apiserver
//! (`service::prepare`, `pod_workspace::prepare`): an `Err(String)` becomes
//! the `message` of an upstream-shaped `422 Invalid` Status — an invalid
//! EXPLICIT value is always rejected, never silently defaulted. Four rule
//! families:
//!
//! 1. **`imagePullPolicy`** — when explicitly present it must be exactly
//!    one of `Always`/`IfNotPresent`/`Never` (upstream field validation).
//!    Omitted stays defaulted (behaviorally irrelevant on host-direct,
//!    T4.15) and is therefore not touched here.
//! 2. **Whole-GPU extended resources** (`common::gpu`, Q61) — quantities in
//!    container `resources.requests`/`limits` must be non-negative integers
//!    (fractional GPUs / MIG / time-slicing are out of scope) and
//!    `requests[x] <= limits[x]` per container when both are present.
//! 3. **GPU visibility-env conflict** (S47 review F1, Q61) — a container
//!    requesting any GPU resource must not explicitly set
//!    `CUDA_VISIBLE_DEVICES`/`HIP_VISIBLE_DEVICES`: the kubelet injects
//!    those from the GPU allocation ledger, so a user value would be
//!    silently overridden at runtime. Non-GPU containers may keep the
//!    vars (e.g. `CUDA_VISIBLE_DEVICES=none` to hide devices).
//! 4. **Host-direct** (Q60/Q61) — any GPU-requesting pod shape — a bare
//!    Pod or an apps/v1 workload template, one shared rule since the S47
//!    review F2 move (see `validate_host_direct`) — must carry the literal
//!    `init-pro.io/rootfs: "/"` annotation; a host-direct Deployment
//!    template must additionally use `spec.strategy.type: Recreate`.

use crate::validate_host_direct::{validate_deployment, validate_gpu_requires_host_direct};
use serde_json::Value;

/// Pod annotation that selects the rootfs source (mirrors the runtime-side
/// SSOT in `kubelet::engine::rootfs`; T4.14/T4.15, Q55/Q60).
pub const ROOTFS_ANNOTATION: &str = "init-pro.io/rootfs";

/// The one literal annotation value that selects host-direct (Q60).
pub const HOST_DIRECT_ROOT: &str = "/";

/// Upstream `imagePullPolicy` field-validation set.
const PULL_POLICIES: [&str; 3] = ["Always", "IfNotPresent", "Never"];

/// Workload kinds whose `spec.template` is a Pod template (apps/v1).
const TEMPLATE_KINDS: [&str; 4] = ["Deployment", "ReplicaSet", "StatefulSet", "DaemonSet"];

/// Admission entry point: validate one incoming object by `(group, kind)`.
///
/// Unknown kinds pass untouched (`Ok(())`) — this seam only owns Pod specs,
/// apps/v1 Pod templates, and the Deployment host-direct rules. `group` is
/// matched case-insensitively (registry parity).
pub fn admit(group: &str, kind: &str, body: &Value) -> Result<(), String> {
    match (group.to_ascii_lowercase().as_str(), kind) {
        ("", "Pod") => validate_pod(Some(body), "spec"),
        ("apps", kind) if TEMPLATE_KINDS.contains(&kind) => {
            validate_pod(body.pointer("/spec/template"), "spec.template.spec")?;
            if kind == "Deployment" {
                validate_deployment(body)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// Validate one pod-shaped owner: a bare Pod (annotations live at
/// `metadata.annotations`) or a workload Pod template (annotations at
/// `spec.template.metadata.annotations`). Runs the container-level rules
/// over `spec` first, then the shared GPU-requires-host-direct rule
/// (S47 review F2) — a malformed quantity reports its specific field
/// error before the cross-field annotation requirement. A missing owner
/// has nothing checkable.
pub fn validate_pod(owner: Option<&Value>, path: &str) -> Result<(), String> {
    let Some(owner) = owner else {
        return Ok(());
    };
    validate_pod_spec(owner.get("spec"), path)?;
    validate_gpu_requires_host_direct(owner, path)
}

/// Validate one Pod spec (`"spec"` for Pods, `"spec.template.spec"` for
/// workload templates). A missing spec has nothing checkable.
pub fn validate_pod_spec(spec: Option<&Value>, path: &str) -> Result<(), String> {
    let Some(spec) = spec else {
        return Ok(());
    };
    for list in ["containers", "initContainers"] {
        if let Some(containers) = spec.get(list).and_then(Value::as_array) {
            for (i, container) in containers.iter().enumerate() {
                let cpath = format!("{path}/{list}[{i}]");
                validate_pull_policy(container, &cpath)?;
                validate_gpu_quantities(container, &cpath)?;
                validate_gpu_env_conflict(container, &cpath)?;
            }
        }
    }
    Ok(())
}

/// An explicit `imagePullPolicy` must be exactly one of the three upstream
/// values; an invalid one is rejected, never silently defaulted (T4.15).
fn validate_pull_policy(container: &Value, path: &str) -> Result<(), String> {
    let Some(raw) = container.get("imagePullPolicy") else {
        return Ok(()); // omitted: stays defaulted
    };
    if raw.as_str().is_some_and(|s| PULL_POLICIES.contains(&s)) {
        return Ok(());
    }
    Err(format!(
        "{path}/imagePullPolicy: Unsupported value: {raw}: supported values: \
         \"Always\", \"IfNotPresent\", \"Never\""
    ))
}

/// One `(resource, raw quantity, parsed value)` triple per GPU entry.
pub(crate) fn gpu_entries<'a>(
    container: &'a Value,
    section: &str,
) -> Vec<(&'a str, &'a Value, Option<f64>)> {
    container
        .pointer(&format!("/resources/{section}"))
        .and_then(Value::as_object)
        .map(|m| {
            m.iter()
                .filter(|(res, _)| common::gpu::is_gpu_resource(res))
                .map(|(res, raw)| (res.as_str(), raw, quantity_value(raw)))
                .collect()
        })
        .unwrap_or_default()
}

/// Parse a JSON quantity (number or Kubernetes quantity string) to `f64`.
fn quantity_value(raw: &Value) -> Option<f64> {
    match raw {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => common::quantity::parse_quantity(s),
        _ => None,
    }
}

/// Whole-GPU quantities (Q61): non-negative integers in requests and
/// limits, and `requests[x] <= limits[x]` per container when both present.
fn validate_gpu_quantities(container: &Value, path: &str) -> Result<(), String> {
    let integer_msg = "must be a non-negative integer (whole-GPU only, Q61)";
    let requests = gpu_entries(container, "requests");
    let limits = gpu_entries(container, "limits");
    for (section, entries) in [("requests", &requests), ("limits", &limits)] {
        for (res, raw, value) in entries {
            let integral =
                matches!(value, Some(v) if v.is_finite() && *v >= 0.0 && v.fract() == 0.0);
            if !integral {
                return Err(format!(
                    "{path}/resources/{section}[{res}]: Invalid value: {raw}: {integer_msg}"
                ));
            }
        }
    }
    for (res, raw, request) in &requests {
        let Some(request) = request else { continue };
        let limit = limits
            .iter()
            .find(|(r, _, _)| r == res)
            .and_then(|(_, _, v)| *v);
        if let Some(limit) = limit {
            if *request > limit {
                return Err(format!(
                    "{path}/resources/requests[{res}]: Invalid value: {raw}: must be less than \
                     or equal to {res} limit of {limit}"
                ));
            }
        }
    }
    Ok(())
}

/// S47 review F1 (Q61): a container requesting any GPU resource (requests
/// OR limits — upstream defaults requests from limits) must not explicitly
/// set a vendor visibility env (`CUDA_VISIBLE_DEVICES` /
/// `HIP_VISIBLE_DEVICES`): the kubelet injects those from the GPU
/// allocation ledger (`kubelet::env::upsert_gpu_env`), so a user value
/// would be silently last-write-wins overridden at runtime. Non-GPU
/// containers may keep the vars (e.g. `CUDA_VISIBLE_DEVICES=none` hiding).
fn validate_gpu_env_conflict(container: &Value, path: &str) -> Result<(), String> {
    let gpu_requested = !gpu_entries(container, "requests").is_empty()
        || !gpu_entries(container, "limits").is_empty();
    if !gpu_requested {
        return Ok(());
    }
    for (i, entry) in container
        .get("env")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
    {
        let Some(name) = entry.get("name").and_then(Value::as_str) else {
            continue; // entries without a name cannot shadow the injection
        };
        let is_visibility = common::gpu::GPU_RESOURCES
            .iter()
            .any(|res| common::gpu::visible_env(res) == Some(name));
        if is_visibility {
            return Err(format!(
                "{path}/env[{i}]: Forbidden: {name} is set by the platform from the GPU \
                 allocation ledger (Q61): remove the explicit entry"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn container(policy: Option<Value>, resources: Value) -> Value {
        let mut c = json!({"name": "c", "image": "i"});
        if let Some(p) = policy {
            c["imagePullPolicy"] = p;
        }
        if !resources.is_null() {
            c["resources"] = resources;
        }
        c
    }
    fn pod(c: Value) -> Value {
        json!({"spec": {"containers": [c]}})
    }

    /// The same pod carrying the literal host-direct annotation (Q60) —
    /// GPU-requesting pods are only admissible in this shape (S47 F2).
    fn hd_pod(c: Value) -> Value {
        json!({"metadata": {"annotations": {ROOTFS_ANNOTATION: HOST_DIRECT_ROOT}},
               "spec": {"containers": [c]}})
    }

    fn admit_pod(c: Value) -> Result<(), String> {
        admit("", "Pod", &pod(c))
    }

    fn admit_hd_pod(c: Value) -> Result<(), String> {
        admit("", "Pod", &hd_pod(c))
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

    // --- imagePullPolicy (2a) ------------------------------------------------

    #[test]
    fn pull_policy_accepts_the_three_upstream_values_and_omission() {
        for good in [
            Some(json!("Always")),
            Some(json!("IfNotPresent")),
            Some(json!("Never")),
            None,
        ] {
            assert!(admit_pod(container(good, Value::Null)).is_ok());
        }
    }

    #[test]
    fn pull_policy_rejects_invalid_explicit_values() {
        for bad in [
            json!("always"),
            json!("ifNotPresent"),
            json!(""),
            json!(1),
            json!(null),
        ] {
            let err = admit_pod(container(Some(bad.clone()), Value::Null)).unwrap_err();
            assert!(
                err.contains("imagePullPolicy: Unsupported value:")
                    && err.contains("\"Always\", \"IfNotPresent\", \"Never\""),
                "message for {bad}: {err}"
            );
        }
    }

    #[test]
    fn pull_policy_is_checked_in_init_containers_and_templates() {
        let body = json!({"spec": {"containers": [{"name": "c", "image": "i"}],
            "initContainers": [{"name": "init", "image": "i", "imagePullPolicy": "sometimes"}]}});
        let err = admit("", "Pod", &body).unwrap_err();
        assert!(
            err.starts_with("spec/initContainers[0]/imagePullPolicy"),
            "{err}"
        );
        let dep = deployment(
            None,
            json!({ROOTFS_ANNOTATION: "/"}),
            container(Some(json!("Always-Not")), Value::Null),
        );
        let err = admit("apps", "Deployment", &dep).unwrap_err();
        assert!(
            err.starts_with("spec.template.spec/containers[0]/imagePullPolicy"),
            "{err}"
        );
    }

    // --- whole-GPU quantities (2b) -------------------------------------------

    #[test]
    fn gpu_quantities_accept_integral_numbers_and_strings() {
        for good in [json!("1"), json!(2), json!("0"), json!("1.0")] {
            let c = container(
                None,
                json!({"limits": {"nvidia.com/gpu": good, "amd.com/gpu": "3"}}),
            );
            assert!(admit_hd_pod(c).is_ok(), "quantity {good} must pass");
        }
    }

    #[test]
    fn gpu_quantities_reject_fractional_negative_and_malformed() {
        for bad in [
            json!("0.5"),
            json!("-1"),
            json!("1.3"),
            json!(0.5),
            json!(-2),
            json!("abc"),
        ] {
            let c = container(None, json!({"limits": {"nvidia.com/gpu": bad.clone()}}));
            let err = admit_pod(c).unwrap_err();
            assert!(
                err.contains(&format!(
                    "resources/limits[nvidia.com/gpu]: Invalid value: {bad}:"
                )),
                "message for {bad}: {err}"
            );
        }
        // The same rejection fires from requests and from templates.
        let err =
            admit_pod(container(None, json!({"requests": {"amd.com/gpu": "1.5"}}))).unwrap_err();
        assert!(err.contains("resources/requests[amd.com/gpu]"), "{err}");
        let err = admit_dep(
            None,
            json!({ROOTFS_ANNOTATION: "/"}),
            container(None, json!({"limits": {"nvidia.com/gpu": "0.5"}})),
        )
        .unwrap_err();
        assert!(err.contains("resources/limits[nvidia.com/gpu]"), "{err}");
    }

    #[test]
    fn gpu_requests_must_not_exceed_limits() {
        let over = json!({"requests": {"nvidia.com/gpu": "2"}, "limits": {"nvidia.com/gpu": "1"}});
        let err = admit_pod(container(None, over)).unwrap_err();
        assert!(
            err.contains("resources/requests[nvidia.com/gpu]: Invalid value: \"2\":")
                && err.contains("must be less than or equal to nvidia.com/gpu limit of 1"),
            "{err}"
        );
        // Equal, requests-only and limits-only all pass (annotation "/").
        for ok in [
            json!({"requests": {"nvidia.com/gpu": "1"}, "limits": {"nvidia.com/gpu": "1"}}),
            json!({"requests": {"nvidia.com/gpu": "1"}}),
            json!({"limits": {"nvidia.com/gpu": "1"}}),
        ] {
            assert!(admit_hd_pod(container(None, ok)).is_ok());
        }
    }

    #[test]
    fn non_gpu_quantities_are_not_whole_device_checked() {
        // Fractional cpu / odd memory stay legal: Q61 constrains only the
        // whole-GPU extended resources.
        let res = json!({"requests": {"cpu": "0.5", "memory": "-1Gi"}, "limits": {"cpu": "100m"}});
        assert!(admit_pod(container(None, res)).is_ok());
    }

    // --- GPU visibility-env conflict (S47 review F1) --------------------------

    #[test]
    fn gpu_container_may_not_set_a_vendor_visibility_env() {
        // The first offending env index is named; requests and limits-only
        // both count as GPU-requesting (upstream defaults requests from
        // limits), for both vendors.
        let mut c = container(None, json!({"requests": {"nvidia.com/gpu": "1"}}));
        c["env"] = json!([{"name": "HARMLESS", "value": "x"}, {"name": "CUDA_VISIBLE_DEVICES", "value": "all"}]);
        let err = admit_hd_pod(c).unwrap_err();
        assert!(
            err.contains("spec/containers[0]/env[1]: Forbidden")
                && err.contains("CUDA_VISIBLE_DEVICES")
                && err.contains("GPU allocation ledger")
                && err.contains("remove the explicit entry"),
            "{err}"
        );
        let mut amd = container(None, json!({"limits": {"amd.com/gpu": "1"}}));
        amd["env"] = json!([{"name": "HIP_VISIBLE_DEVICES", "value": "0"}]);
        let err = admit_hd_pod(amd).unwrap_err();
        assert!(
            err.contains("spec/containers[0]/env[0]: Forbidden")
                && err.contains("HIP_VISIBLE_DEVICES"),
            "{err}"
        );
    }

    #[test]
    fn visibility_env_on_a_non_gpu_container_stays_legal() {
        // e.g. CUDA_VISIBLE_DEVICES=none deliberately hiding devices: the
        // var is only platform-owned on GPU-requesting containers.
        let mut c = container(None, Value::Null);
        c["env"] = json!([{"name": "CUDA_VISIBLE_DEVICES", "value": "none"}]);
        assert!(admit_pod(c).is_ok());
    }

    #[test]
    fn env_conflict_covers_init_containers_and_odd_env_shapes() {
        let body = json!({
            "metadata": {"annotations": {ROOTFS_ANNOTATION: HOST_DIRECT_ROOT}},
            "spec": {"containers": [{"name": "c", "image": "i"}],
                     "initContainers": [{"name": "init", "image": "i",
                         "resources": {"limits": {"nvidia.com/gpu": "1"}},
                         "env": [{"value": "nameless"}, {"name": "CUDA_VISIBLE_DEVICES"}]}]}
        });
        let err = admit("", "Pod", &body).unwrap_err();
        assert!(
            err.contains("spec/initContainers[0]/env[1]: Forbidden"),
            "{err}"
        );
        // Entries without a name never trip the rule, and a GPU container
        // without an env array at all is fine (the platform injects one).
        let ok = json!({
            "metadata": {"annotations": {ROOTFS_ANNOTATION: HOST_DIRECT_ROOT}},
            "spec": {"containers": [{"name": "c", "image": "i",
                "resources": {"requests": {"nvidia.com/gpu": "1"}},
                "env": [{"value": "nameless"}]}]}
        });
        assert!(admit("", "Pod", &ok).is_ok());
    }

    // --- dispatch --------------------------------------------------------------

    #[test]
    fn admit_validates_pod_and_apps_templates_and_ignores_the_rest() {
        // apps/v1 template kinds share the pod-spec rules (group is
        // case-insensitive, registry parity).
        let mk = |kind: &str| {
            json!({
                "kind": kind,
                "spec": {"template": {"spec": {"containers": [
                    {"name": "c", "image": "i", "imagePullPolicy": "nope"}
                ]}}}
            })
        };
        for kind in TEMPLATE_KINDS {
            assert!(
                admit("Apps", kind, &mk(kind)).is_err(),
                "{kind} template validated"
            );
        }
        // Unknown kinds (and missing specs) are untouched.
        assert!(admit("", "ConfigMap", &json!({"metadata": {"name": "m"}})).is_ok());
        assert!(admit("argoproj.io", "Workflow", &json!({"spec": {}})).is_ok());
        assert!(admit("", "Pod", &json!({"metadata": {"name": "p"}})).is_ok());
        assert!(admit("apps", "Deployment", &json!({"metadata": {"name": "d"}})).is_ok());
    }
}
