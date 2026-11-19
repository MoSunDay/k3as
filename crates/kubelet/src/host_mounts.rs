//! `init-pro.io/host-mounts` pod annotation (T4.14, Q55): extra host
//! directory bind mounts — `container-path=host-path[:ro]`, comma
//! separated (the `init-pro.io/anyhome-mounts` spelling, mirrored).
//!
//! Parsing and pod-level validation are PURE (no fs IO): the mount table
//! stays the visibility boundary (nsrun NEWNS private binds + the
//! `inroot` secure walk), and bind failure IS the permission verdict —
//! the node process creds decide, with no pre-flight ACL guessing (Q55).
//! Existence is enforced at execution (the create-time source check in
//! [`crate::exec`]); a bad annotation blocks the pod's container creates
//! for the cycle and posts a `CreateContainerConfigError` event via
//! [`emit_error_events`], mirroring the env-resolution seam (T4.7).

use std::collections::BTreeMap;

use serde_json::json;
use serde_json::Value;

use crate::http::HttpJson;
use crate::objects::PodView;
use runtime::cri_json::Mount;

/// Pod annotation (Q55): `container-path=host-path[:ro],...`.
pub const HOST_MOUNTS_ANNOTATION: &str = "init-pro.io/host-mounts";

/// One parsed host mount (Q55): what binds where, and whether the bind
/// is hardened read-only (`mount_setattr`, see nsrun `apply_bind`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostMount {
    pub container_path: String,
    pub host_path: String,
    pub read_only: bool,
}

/// Absolute path guard (Q55): both halves must be absolute; `..` segments
/// are rejected early (nsrun's `inroot` walk re-validates anyway).
fn absolute_path_ok(p: &str) -> bool {
    p.starts_with('/') && !p.split('/').any(|seg| seg == "..")
}

/// Parse the annotation value (pure format validation, Q55):
/// `container-path=host-path[:ro]`, comma separated; empty items are
/// tolerated (trailing commas). Duplicate container paths are rejected —
/// one bind per container path.
pub fn parse_host_mounts(raw: &str) -> Result<Vec<HostMount>, String> {
    let mut out = Vec::new();
    for item in raw.split(',') {
        let item = item.trim();
        if item.is_empty() {
            continue;
        }
        let (cpath, rest) = item.split_once('=').ok_or_else(|| {
            format!("item {item:?} lacks '=' (expected container-path=host-path[:ro|:rw])")
        })?;
        // Mode suffix mirrors the anyhome-mounts convention (Q43): exactly
        // `ro` or `rw`; any other colon tail names itself in the error.
        let (hpath, ro) = match rest.split(':').collect::<Vec<_>>()[..] {
            [path] => (path, false),
            [path, "ro"] => (path, true),
            [path, "rw"] => (path, false),
            _ => {
                return Err(format!(
                    "item {item:?}: host path must be one path plus an optional :ro|:rw"
                ))
            }
        };
        let (cpath, hpath) = (cpath.trim(), hpath.trim());
        if !absolute_path_ok(cpath) {
            return Err(format!("container path {cpath:?} must be absolute"));
        }
        if !absolute_path_ok(hpath) {
            return Err(format!("host path {hpath:?} must be absolute"));
        }
        if out.iter().any(|m: &HostMount| m.container_path == cpath) {
            return Err(format!("duplicate container path {cpath:?}"));
        }
        out.push(HostMount {
            container_path: cpath.to_string(),
            host_path: hpath.to_string(),
            read_only: ro,
        });
    }
    Ok(out)
}

/// Parsed host mounts for one pod (Q55): absent annotation = empty;
/// malformed = the error that blocks the pod's creates this cycle.
/// Collisions with the pod's own `volumeMounts` paths are pod-level
/// rejections ("同 pod 内冲突拒绝").
pub fn collect(view: &PodView) -> Result<Vec<HostMount>, String> {
    let Some(raw) = view.annotations.get(HOST_MOUNTS_ANNOTATION) else {
        return Ok(Vec::new());
    };
    let mounts = parse_host_mounts(raw)
        .map_err(|e| format!("invalid {HOST_MOUNTS_ANNOTATION} annotation: {e}"))?;
    for m in &mounts {
        let claimed = view.containers.iter().any(|c| {
            c.volume_mounts
                .iter()
                .any(|vm| vm.mount_path == m.container_path)
        });
        if claimed {
            return Err(format!(
                "invalid {HOST_MOUNTS_ANNOTATION} annotation: container path \
                 {} collides with a volumeMount",
                m.container_path
            ));
        }
    }
    Ok(mounts)
}

/// Fold validated host mounts into CRI mounts (Q55) — the hostdir kind of
/// the mount-resolution table (see [`crate::volumes::resolve_mounts`]).
pub fn cri_mounts(mounts: &[HostMount]) -> Vec<Mount> {
    mounts
        .iter()
        .map(|m| Mount {
            container_path: m.container_path.clone(),
            host_path: m.host_path.clone(),
            readonly: m.read_only,
        })
        .collect()
}

/// Post upstream-parity `CreateContainerConfigError` events for pods whose
/// `host-mounts` annotation failed validation (Q55; mirrors the T4.7 env
/// seam: deduped by pod+reason, one event per failure, not per cycle).
pub async fn emit_error_events(
    client: &HttpJson,
    node_name: &str,
    wanted: &BTreeMap<String, PodView>,
    last: &mut BTreeMap<String, String>,
    now: &str,
) {
    let mut live = BTreeMap::new();
    for (key, view) in wanted {
        if view.deleted {
            continue;
        }
        let Err(reason) = collect(view) else {
            continue; // no annotation, or a valid one
        };
        let dedup = key.clone();
        live.insert(dedup.clone(), reason.clone());
        if last.get(&dedup).is_some_and(|r| r == &reason) {
            continue;
        }
        let event = host_mounts_error_event(
            &view.namespace,
            &view.name,
            &view.uid,
            &reason,
            node_name,
            now,
        );
        let path = format!("/api/v1/namespaces/{}/events", view.namespace);
        match client.post_json(&path, &event).await {
            Ok((201, _)) => {
                last.insert(dedup, reason);
            }
            Ok((code, body)) => tracing::debug!(
                target: "init-pro",
                code = code, %body,
                "kubelet: host-mounts error event POST rejected"
            ),
            Err(e) => tracing::debug!(
                target: "init-pro",
                error = %e,
                "kubelet: host-mounts error event POST failed"
            ),
        }
    }
    last.retain(|k, _| live.contains_key(k));
}

fn host_mounts_error_event(
    namespace: &str,
    pod: &str,
    uid: &str,
    message: &str,
    node: &str,
    now: &str,
) -> Value {
    json!({
        "apiVersion": "v1",
        "kind": "Event",
        "metadata": {
            "generateName": format!("{pod}."),
            "namespace": namespace,
        },
        "involvedObject": {
            "kind": "Pod",
            "apiVersion": "v1",
            "namespace": namespace,
            "name": pod,
            "uid": uid,
        },
        "reason": "CreateContainerConfigError",
        "message": message,
        "type": "Warning",
        "count": 1,
        "firstTimestamp": now,
        "lastTimestamp": now,
        "source": {"component": "kubelet", "host": node},
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rw_ro_and_tolerates_trailing_comma() {
        let ms = parse_host_mounts("/data=/srv/app, /conf=/etc/conf:ro,").unwrap();
        assert_eq!(
            ms,
            vec![
                HostMount {
                    container_path: "/data".into(),
                    host_path: "/srv/app".into(),
                    read_only: false
                },
                HostMount {
                    container_path: "/conf".into(),
                    host_path: "/etc/conf".into(),
                    read_only: true
                },
            ]
        );
    }

    #[test]
    fn rejects_relative_paths_traversal_dups_and_garbage() {
        assert!(parse_host_mounts("data=/srv").is_err());
        assert!(parse_host_mounts("/data=srv").is_err());
        assert!(parse_host_mounts("/data=/srv/../etc").is_err());
        assert!(parse_host_mounts("/a=/h,/a=/h2").is_err());
        assert!(parse_host_mounts("/a").is_err());
        assert!(parse_host_mounts("/a=/h:xo").is_err());
        assert!(parse_host_mounts("/a=/h:ro:rw").is_err());
        // :rw is the explicit spelling of the default (anyhome parity).
        assert!(!parse_host_mounts("/a=/h:rw").unwrap()[0].read_only);
        assert!(parse_host_mounts("").unwrap().is_empty());
    }

    #[test]
    fn cri_mounts_map_one_to_one() {
        let ms = parse_host_mounts("/d=/h:ro").unwrap();
        let mounts = cri_mounts(&ms);
        assert_eq!(mounts.len(), 1);
        assert_eq!(mounts[0].container_path, "/d");
        assert_eq!(mounts[0].host_path, "/h");
        assert!(mounts[0].readonly);
    }
}
