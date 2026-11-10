//! Per-Pod anyhome NFS mount defaulting at the API boundary (T7.5, Q43).
//!
//! Create-time ([`crate::collection::do_create`]) and apply-time
//! ([`crate::apply::do_apply`]) defaulting for the `init-pro.io/anyhome`
//! and `init-pro.io/anyhome-mounts` Pod annotations: with the anyhome plane
//! on, they become concrete `spec.volumes[]` nfs entries plus per-container
//! `volumeMounts`. The injection mirrors the Workflow engine's step-pod
//! `/workspace` auto-mount (Q36) under the same **A4** rule — a user's own
//! declarations win, injection only fills the gaps.
//!
//! Pure JSON surgery over the request body: the annotation parsers, the
//! `/pod/<ns>/<name>` path builder and the volume-name derivation are the
//! SSOT in `common::workspace`; this module only composes them
//! idempotently. The dedup rules in [`ensure_pod_volume`] /
//! [`ensure_container_mount`] (same volume `name`, same nfs `path`, same
//! mount `name`/`mountPath`) make [`prepare`] a no-op on the second call —
//! which is what keeps repeated server-side applies stable.
//! `workspace_nfs == None` (`--disable-anyhome`) makes everything inert:
//! `Ok(())` immediately, no validation, body untouched. Un-annotated Pods
//! are untouched either way, so the golden surface (T0.6) is unaffected.
use common::{
    anyhome_mount_volume_name, parse_anyhome_mode, parse_anyhome_mounts, AnyhomeMode,
    ANYHOME_ANNOTATION, ANYHOME_MOUNTS_ANNOTATION, ANYHOME_VOLUME, MOUNT_TARGET,
};
use serde_json::{json, Map, Value};

/// Step-2 guard failure: the Pod identity must be present and path-safe.
const IDENTITY_MSG: &str = "init-pro.io/anyhome: pod requires metadata.namespace and metadata.name";

/// Default a Pod body with anyhome volumes + mounts (T7.5, Q43).
///
/// 1. `workspace_nfs == None` -> `Ok(())` immediately, body untouched, no
///    validation (the plane is off; annotations are inert).
/// 2. `metadata.namespace`/`metadata.name` must be non-empty and `/`-free.
/// 3. `init-pro.io/anyhome` = `rw|ro|true` -> one `anyhome` nfs volume at
///    `/pod/<ns>/<name>` + a `/workspace` mount in every container lacking
///    one; `false` -> no workspace injection (the mounts annotation stays
///    independently in effect); anything else -> `Err`.
/// 4. `init-pro.io/anyhome-mounts` -> one derived-name nfs volume + mount
///    per parsed triple; a malformed value -> `Err`.
///
/// Idempotent: a second call over an already-prepared body changes nothing.
pub(crate) fn prepare(workspace_nfs: Option<&str>, body: &mut Value) -> Result<(), String> {
    // 1. Plane off (--disable-anyhome): annotations inert, bytes untouched.
    let Some(addr) = workspace_nfs else {
        return Ok(());
    };
    // 2. Identity guard (pod_workspace_path is Some by construction below).
    let ns = str_at(body, "/metadata/namespace").to_string();
    let name = str_at(body, "/metadata/name").to_string();
    if ns.is_empty() || name.is_empty() || ns.contains('/') || name.contains('/') {
        return Err(IDENTITY_MSG.to_string());
    }
    // 3. The workspace annotation: one generic volume + /workspace mount.
    if let Some(raw) = annotation(body, ANYHOME_ANNOTATION) {
        match raw.as_str().and_then(parse_anyhome_mode) {
            Some(mode @ (AnyhomeMode::Rw | AnyhomeMode::Ro)) => {
                inject_workspace(body, addr, &ns, &name, mode == AnyhomeMode::Ro);
            }
            // Explicit opt-out: no workspace injection; the extra-mounts
            // annotation below stays independently in effect.
            Some(AnyhomeMode::Off) => {}
            None => {
                return Err(format!(
                    "invalid {ANYHOME_ANNOTATION} annotation {:?}: expected rw|ro|true|false",
                    annotation_text(raw),
                ))
            }
        }
    }
    // 4. The extra-mounts annotation: one volume + mount per triple.
    if let Some(raw) = annotation(body, ANYHOME_MOUNTS_ANNOTATION) {
        match raw.as_str().map(parse_anyhome_mounts) {
            Some(Ok(mounts)) => {
                for m in mounts {
                    let volume_name = anyhome_mount_volume_name(&m.container_path);
                    ensure_pod_volume(body, &volume_name, addr, &m.server_path, m.read_only);
                    for_each_container(body, |container| {
                        ensure_container_mount(container, &volume_name, &m.container_path)
                    });
                }
            }
            Some(Err(e)) => {
                return Err(format!(
                    "invalid {ANYHOME_MOUNTS_ANNOTATION} annotation: {e}"
                ))
            }
            None => {
                return Err(format!(
                    "invalid {ANYHOME_MOUNTS_ANNOTATION} annotation: value must be a string"
                ))
            }
        }
    }
    Ok(())
}

/// Inject the pod's own workspace volume + the shared `/workspace` mount.
fn inject_workspace(body: &mut Value, addr: &str, ns: &str, name: &str, read_only: bool) {
    // Some() is guaranteed by the step-2 guard; skip rather than guess if
    // that invariant is somehow broken.
    let Some(path) = common::pod_workspace_path(ns, name) else {
        return;
    };
    ensure_pod_volume(body, ANYHOME_VOLUME, addr, &path, read_only);
    for_each_container(body, |container| {
        ensure_container_mount(container, ANYHOME_VOLUME, MOUNT_TARGET)
    });
}

/// Append `{"name": N, "nfs": {server, path, readOnly}}` to `spec.volumes`
/// (array created when absent) UNLESS an existing volume has the same
/// `name` OR the same nfs `path` — A4 (user declarations win) + idempotent
/// re-preparation; the same-path rule also makes Workflow-engine step pods
/// (which carry the `workspace` nfs volume) collide safely.
fn ensure_pod_volume(body: &mut Value, name: &str, addr: &str, path: &str, read_only: bool) {
    let taken = body
        .pointer("/spec/volumes")
        .and_then(Value::as_array)
        .is_some_and(|volumes| volumes.iter().any(|v| volume_taken(v, name, path)));
    if taken {
        return;
    }
    let Some(obj) = body.as_object_mut() else {
        return;
    };
    let spec = obj
        .entry("spec")
        .or_insert_with(|| Value::Object(Map::new()));
    let Some(spec) = spec.as_object_mut() else {
        return;
    };
    let volumes = spec
        .entry("volumes")
        .or_insert_with(|| Value::Array(Vec::new()));
    if let Some(arr) = volumes.as_array_mut() {
        let nfs = json!({ "server": addr, "path": path, "readOnly": read_only });
        arr.push(json!({ "name": name, "nfs": nfs }));
    }
}

/// A4 + dedup predicate for [`ensure_pod_volume`].
fn volume_taken(existing: &Value, name: &str, path: &str) -> bool {
    existing.get("name").and_then(Value::as_str) == Some(name)
        || existing.pointer("/nfs/path").and_then(Value::as_str) == Some(path)
}

/// Append `{"name": N, "mountPath": P}` to the container's `volumeMounts`
/// (array created when absent) unless an existing entry already claims the
/// same `mountPath` or the same `name` (A4: user declarations win).
fn ensure_container_mount(container: &mut Value, name: &str, mount_path: &str) {
    let taken = container
        .get("volumeMounts")
        .and_then(Value::as_array)
        .is_some_and(|mounts| {
            mounts.iter().any(|m| {
                m.get("mountPath").and_then(Value::as_str) == Some(mount_path)
                    || m.get("name").and_then(Value::as_str) == Some(name)
            })
        });
    if taken {
        return;
    }
    let Some(obj) = container.as_object_mut() else {
        return;
    };
    let mounts = obj
        .entry("volumeMounts")
        .or_insert_with(|| Value::Array(Vec::new()));
    if let Some(arr) = mounts.as_array_mut() {
        arr.push(json!({ "name": name, "mountPath": mount_path }));
    }
}

/// Run `inject` over every `spec.containers[]` (none when absent — the
/// volume alone is harmless and deterministic).
fn for_each_container(body: &mut Value, mut inject: impl FnMut(&mut Value)) {
    if let Some(containers) = body
        .pointer_mut("/spec/containers")
        .and_then(Value::as_array_mut)
    {
        for container in containers {
            inject(container);
        }
    }
}

/// A present, non-null annotation value (absent/JSON null -> None).
fn annotation<'a>(body: &'a Value, key: &str) -> Option<&'a Value> {
    body.pointer("/metadata/annotations")
        .and_then(|a| a.get(key))
        .filter(|v| !v.is_null())
}

/// String at a JSON pointer, "" when absent/non-string.
fn str_at<'a>(body: &'a Value, pointer: &str) -> &'a str {
    body.pointer(pointer).and_then(Value::as_str).unwrap_or("")
}

/// Error-message rendering of an annotation value (raw JSON text).
fn annotation_text(raw: &Value) -> String {
    raw.as_str()
        .map(str::to_string)
        .unwrap_or_else(|| raw.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const ADDR: &str = "127.0.0.1:11111";

    /// Minimal Pod body; `annotations`/`mounts` are `Value::Null` when the
    /// caller wants the keys absent.
    fn pod(annotations: Value, mounts: Value) -> Value {
        let mut metadata = json!({ "name": "web", "namespace": "default" });
        if !annotations.is_null() {
            metadata["annotations"] = annotations;
        }
        let mut container = json!({ "name": "app", "image": "busybox" });
        if !mounts.is_null() {
            container["volumeMounts"] = mounts;
        }
        json!({
            "apiVersion": "v1", "kind": "Pod", "metadata": metadata,
            "spec": { "containers": [container] },
        })
    }

    #[test]
    fn rw_happy_path_injects_volume_and_mount() {
        let mut body = pod(json!({ "init-pro.io/anyhome": "rw" }), Value::Null);
        prepare(Some(ADDR), &mut body).unwrap();
        assert_eq!(
            body["spec"]["volumes"],
            json!([{
                "name": "anyhome",
                "nfs": { "server": ADDR, "path": "/pod/default/web", "readOnly": false },
            }])
        );
        assert_eq!(
            body["spec"]["containers"][0]["volumeMounts"],
            json!([{ "name": "anyhome", "mountPath": "/workspace" }])
        );
    }

    #[test]
    fn ro_mode_marks_the_volume_read_only() {
        let mut body = pod(json!({ "init-pro.io/anyhome": "ro" }), Value::Null);
        prepare(Some(ADDR), &mut body).unwrap();
        assert_eq!(body["spec"]["volumes"][0]["nfs"]["readOnly"], json!(true));
        assert_eq!(
            body["spec"]["containers"][0]["volumeMounts"][0],
            json!({ "name": "anyhome", "mountPath": "/workspace" })
        );
    }

    #[test]
    fn mounts_annotation_injects_derived_named_volumes() {
        let mut body = pod(
            json!({ "init-pro.io/anyhome-mounts": "/team/shared:/shared:ro,/cache:/cache" }),
            Value::Null,
        );
        prepare(Some(ADDR), &mut body).unwrap();
        assert_eq!(
            body["spec"]["volumes"],
            json!([
                { "name": "anyhome-shared",
                  "nfs": { "server": ADDR, "path": "/team/shared", "readOnly": true } },
                { "name": "anyhome-cache",
                  "nfs": { "server": ADDR, "path": "/cache", "readOnly": false } },
            ])
        );
        assert_eq!(
            body["spec"]["containers"][0]["volumeMounts"],
            json!([
                { "name": "anyhome-shared", "mountPath": "/shared" },
                { "name": "anyhome-cache", "mountPath": "/cache" },
            ])
        );
    }

    #[test]
    fn a4_user_mount_at_workspace_wins() {
        // The container declares its own mount at /workspace: the injected
        // mount is skipped, but the pod-level `anyhome` volume name is free,
        // so the volume itself is still added (helper behavior, pinned).
        let mut body = pod(
            json!({ "init-pro.io/anyhome": "rw" }),
            json!([{ "name": "user-vol", "mountPath": "/workspace" }]),
        );
        prepare(Some(ADDR), &mut body).unwrap();
        assert_eq!(
            body["spec"]["containers"][0]["volumeMounts"],
            json!([{ "name": "user-vol", "mountPath": "/workspace" }])
        );
        assert_eq!(body["spec"]["volumes"].as_array().map(Vec::len), Some(1));
        assert_eq!(body["spec"]["volumes"][0]["name"], json!("anyhome"));
    }

    #[test]
    fn a4_existing_volume_suppresses_volume_injection() {
        // A pod-level volume literally named `anyhome` (user-declared) or an
        // nfs volume with the same server path (Workflow-engine collision)
        // both suppress the volume append; the mount still lands.
        for existing in [
            json!({ "name": "anyhome", "emptyDir": {} }),
            json!({ "name": "workspace", "nfs": { "server": "other:2049", "path": "/pod/default/web" } }),
        ] {
            let mut body = pod(json!({ "init-pro.io/anyhome": "rw" }), Value::Null);
            body["spec"]["volumes"] = json!([existing]);
            prepare(Some(ADDR), &mut body).unwrap();
            assert_eq!(body["spec"]["volumes"].as_array().map(Vec::len), Some(1));
            assert_eq!(
                body["spec"]["containers"][0]["volumeMounts"],
                json!([{ "name": "anyhome", "mountPath": "/workspace" }])
            );
        }
    }

    #[test]
    fn double_prepare_is_a_no_op() {
        let mut body = pod(
            json!({ "init-pro.io/anyhome": "rw", "init-pro.io/anyhome-mounts": "/cache:/cache" }),
            Value::Null,
        );
        prepare(Some(ADDR), &mut body).unwrap();
        let once = body.clone();
        prepare(Some(ADDR), &mut body).unwrap();
        assert_eq!(body, once);
        assert_eq!(body["spec"]["volumes"].as_array().map(Vec::len), Some(2));
    }

    #[test]
    fn none_endpoint_leaves_the_body_byte_identical() {
        let mut body = pod(json!({ "init-pro.io/anyhome": "banana" }), Value::Null);
        let before = serde_json::to_string(&body).unwrap();
        prepare(None, &mut body).unwrap();
        assert_eq!(serde_json::to_string(&body).unwrap(), before);
    }

    #[test]
    fn invalid_mode_value_is_rejected() {
        let mut body = pod(json!({ "init-pro.io/anyhome": "banana" }), Value::Null);
        assert_eq!(
            prepare(Some(ADDR), &mut body),
            Err("invalid init-pro.io/anyhome annotation \"banana\": \
                 expected rw|ro|true|false"
                .to_string())
        );
    }

    #[test]
    fn invalid_mounts_value_is_rejected() {
        let mut body = pod(
            json!({ "init-pro.io/anyhome-mounts": "/cache" }),
            Value::Null,
        );
        let err = prepare(Some(ADDR), &mut body).unwrap_err();
        assert!(
            err.starts_with("invalid init-pro.io/anyhome-mounts annotation: "),
            "got {err}"
        );
    }

    #[test]
    fn unannotated_pod_is_untouched() {
        let mut body = pod(Value::Null, Value::Null);
        prepare(Some(ADDR), &mut body).unwrap();
        assert!(body["spec"].get("volumes").is_none());
        assert!(body["spec"]["containers"][0].get("volumeMounts").is_none());
    }

    #[test]
    fn off_mode_keeps_extra_mounts_in_effect() {
        let mut body = pod(
            json!({ "init-pro.io/anyhome": "false", "init-pro.io/anyhome-mounts": "/cache:/cache" }),
            Value::Null,
        );
        prepare(Some(ADDR), &mut body).unwrap();
        assert_eq!(body["spec"]["volumes"].as_array().map(Vec::len), Some(1));
        assert_eq!(body["spec"]["volumes"][0]["name"], json!("anyhome-cache"));
    }

    #[test]
    fn missing_identity_is_rejected() {
        let mut body = pod(json!({ "init-pro.io/anyhome": "rw" }), Value::Null);
        let meta = body["metadata"].as_object_mut().unwrap();
        meta.remove("namespace");
        assert_eq!(
            prepare(Some(ADDR), &mut body),
            Err(IDENTITY_MSG.to_string())
        );
        body["metadata"]["name"] = json!("a/b");
        assert_eq!(
            prepare(Some(ADDR), &mut body),
            Err(IDENTITY_MSG.to_string())
        );
    }
}
