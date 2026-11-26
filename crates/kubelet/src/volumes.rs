//! Pod volume materialization (TODO **T4.2**, Sprint 20 S2, Scope B).
//!
//! emptyDir dirs and configMap/secret key files live under
//! `<log-root>/<uid>/volumes/...`. ConfigMap/secret content is refreshed
//! in place via a deterministic FNV-1a fingerprint marker (`.fingerprint`)
//! — the sync loop re-fetches the referenced object each cycle and rewrites
//! files ONLY when the marker differs, so there is no churn and no watcher
//! (plan R5: avoids informer races entirely). Errors are values; one bad
//! fetch never fails the whole pass — the missing host dir makes the CRI
//! create fail and the cycle retries.
//!
//! nfs volumes (T-anyhome, Q36) bind the anyhome NFSv3 export at
//! `<log-root>/<uid>/volumes/nfs/<name>` via a host `mount.nfs` subprocess
//! (mechanics live in [`crate::nfs_mount`]); cleanup unmounts BEFORE
//! deleting the tree so the server-side files always survive.
//!
//! Cleanup (C4) releases the WHOLE per-pod directory `<log-root>/<uid>`,
//! not only `volumes/`: once the pod's sandbox is gone the only other
//! content is its date-shaped pod-log dirs (T4.6, Q39), which otherwise
//! accumulated one directory per pod ever scheduled. The uid is validated
//! as a bare path component BEFORE any filesystem action, and a per-pod
//! dir that is not a plain directory is refused rather than followed.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use runtime::cri_json::Mount;
use serde_json::Value;

use crate::http::HttpJson;
use crate::nfs_mount;
use crate::objects::{ContainerSpec, PodView, VolumeKind, VolumeSpec};

/// Marker file holding the hex fingerprint of the last-written key files.
const FINGERPRINT_FILE: &str = ".fingerprint";

// ---- paths ----------------------------------------------------------------

/// `<log-root>/<uid>/volumes` — the root of every volume of one pod.
pub fn volume_root(log_root: &Path, uid: &str) -> PathBuf {
    log_root.join(uid).join("volumes")
}

/// `<log-root>/<uid>` -- one pod's whole per-uid tree: the date-shaped
/// pod-log dirs (T4.6, Q39) plus [`volume_root`]. Cleanup removes exactly
/// this directory once the pod is gone (C4).
pub fn pod_dir(log_root: &Path, uid: &str) -> PathBuf {
    log_root.join(uid)
}

/// Longest uid accepted by [`is_safe_uid`] (a UUID is 36; the bound only
/// keeps a hostile name from being a path-length edge case).
const MAX_UID_LEN: usize = 128;

/// A uid is only ever used as ONE path component under the log root and
/// cleanup `remove_dir_all`s it (C4), so the accepted shape is a bare
/// component: 1..=[`MAX_UID_LEN`] ASCII bytes of `[A-Za-z0-9._-]`, never
/// `.` or `..`. The apiserver stamps UUIDs on create (T1.2), so anything
/// else -- empty, `/`, `\`, NUL, the synthetic `no-uid-<ns>/<name>`
/// fallback -- is refused BEFORE any filesystem action: a refused cleanup
/// leaks a directory and surfaces a warning, it never deletes.
fn is_safe_uid(uid: &str) -> bool {
    !uid.is_empty()
        && uid.len() <= MAX_UID_LEN
        && uid != "."
        && uid != ".."
        && uid
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// Directory of one emptyDir volume.
pub fn empty_dir_path(log_root: &Path, uid: &str, name: &str) -> PathBuf {
    volume_dir(log_root, uid, "emptyDir", name)
}

/// Host path of one configMap key file (the volume dir is its parent).
pub fn config_map_path(log_root: &Path, uid: &str, name: &str, key: &str) -> PathBuf {
    volume_dir(log_root, uid, "configMap", name).join(key)
}

/// Host path of one secret key file (the volume dir is its parent).
pub fn secret_path(log_root: &Path, uid: &str, name: &str, key: &str) -> PathBuf {
    volume_dir(log_root, uid, "secret", name).join(key)
}

/// Host mountpoint of one nfs volume (T-anyhome, Q36): the `mount.nfs`
/// target under `<log-root>/<uid>/volumes/nfs/<name>`.
pub fn nfs_volume_path(log_root: &Path, uid: &str, name: &str) -> PathBuf {
    volume_dir(log_root, uid, "nfs", name)
}

fn volume_dir(log_root: &Path, uid: &str, kind: &str, name: &str) -> PathBuf {
    volume_root(log_root, uid).join(kind).join(name)
}

// ---- ensure / cleanup -----------------------------------------------------

/// Materialize every desired pod's volumes: emptyDir dirs are created
/// unconditionally; configMap/secret volumes are fetched from the
/// apiserver and their key files (re)written only when the content
/// fingerprint changed. Per-volume failures are logged and skipped so one
/// 404 (or a flaky fetch) never blocks the rest of the pass.
pub async fn ensure_volumes(
    client: &HttpJson,
    wanted: &BTreeMap<String, PodView>,
    log_root: &Path,
) -> Result<(), String> {
    for view in wanted.values() {
        if view.deleted {
            continue; // being torn down; cleanup_volumes owns the dir
        }
        for vol in &view.volumes {
            if let Err(e) = ensure_one_volume(client, view, vol, log_root).await {
                tracing::warn!(
                    target: "init-pro",
                    pod = %view.key, volume = %vol.name, error = %e,
                    "kubelet: volume materialization failed (cycle retries)"
                );
            }
        }
    }
    Ok(())
}

async fn ensure_one_volume(
    client: &HttpJson,
    view: &PodView,
    vol: &VolumeSpec,
    log_root: &Path,
) -> Result<(), String> {
    match &vol.kind {
        VolumeKind::EmptyDir => {
            tokio::fs::create_dir_all(empty_dir_path(log_root, &view.uid, &vol.name))
                .await
                .map_err(|e| format!("create emptyDir {}: {e}", vol.name))
        }
        VolumeKind::ConfigMap { name } => {
            let path = format!("/api/v1/namespaces/{}/configmaps/{name}", view.namespace);
            refresh_files(client, &path, view, log_root, "configMap", name).await
        }
        VolumeKind::Secret { name } => {
            let path = format!("/api/v1/namespaces/{}/secrets/{name}", view.namespace);
            refresh_files(client, &path, view, log_root, "secret", name).await
        }
        VolumeKind::Nfs { server, path, .. } => {
            let dir = nfs_volume_path(log_root, &view.uid, &vol.name);
            tokio::fs::create_dir_all(&dir)
                .await
                .map_err(|e| format!("create nfs dir {}: {e}", dir.display()))?;
            let mounts = std::fs::read_to_string(PROC_MOUNTS)
                .map_err(|e| format!("read {PROC_MOUNTS}: {e}"))?;
            if nfs_mount::is_mountpoint(&mounts, &dir) {
                return Ok(()); // already mounted: idempotent re-converge
            }
            // The bare name lets PATH lookup find the real helper; tests
            // inject a fake binary through nfs_mount, never this arm.
            nfs_mount::mount_nfs(Path::new("mount.nfs"), server, path, &dir).await
        }
    }
}

/// Fetch one configMap/secret, fingerprint its `data`, and rewrite the key
/// files only when the marker differs (in-place refresh, no churn). The
/// volume dir is named by the OBJECT name (not the pod volume name) —
/// emptyDir dirs use the volume name (see [`empty_dir_path`]).
async fn refresh_files(
    client: &HttpJson,
    path: &str,
    view: &PodView,
    log_root: &Path,
    kind: &str,
    object_name: &str,
) -> Result<(), String> {
    let (code, body) = client
        .get_json(path)
        .await
        .map_err(|e| format!("GET {path}: {e}"))?;
    if code != 200 {
        return Err(format!("GET {path} -> HTTP {code}"));
    }
    let dir = volume_dir(log_root, &view.uid, kind, object_name);
    apply_if_changed(&dir, &data_map(&body)).await.map(|_| ())
}

/// Compare the fingerprint of `data` against the `.fingerprint` marker and
/// (re)write the key files + marker when it differs. Returns true when
/// files were written; `Ok(false)` is the steady-state no-op. The
/// fingerprint covers only safe keys — unsafe entries never land on disk,
/// so they must not drive rewrites either.
async fn apply_if_changed(dir: &Path, data: &BTreeMap<String, String>) -> Result<bool, String> {
    let safe: BTreeMap<String, String> = data
        .iter()
        .filter(|(k, _)| is_safe_key(k))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let fp = format!("{:016x}", fingerprint(&safe));
    let marker = dir.join(FINGERPRINT_FILE);
    if tokio::fs::read_to_string(&marker)
        .await
        .map(|s| s.trim() == fp)
        .unwrap_or(false)
    {
        return Ok(false);
    }
    tokio::fs::create_dir_all(dir)
        .await
        .map_err(|e| format!("create volume dir {}: {e}", dir.display()))?;
    for (key, value) in &safe {
        tokio::fs::write(dir.join(key), value.as_bytes())
            .await
            .map_err(|e| format!("write {}: {e}", dir.join(key).display()))?;
    }
    tokio::fs::write(&marker, &fp)
        .await
        .map_err(|e| format!("write marker {}: {e}", marker.display()))?;
    Ok(true)
}

/// `data` as a sorted map; a missing `data` field or non-string values
/// contribute nothing (empty data -> marker-only volume, still detected).
fn data_map(body: &Value) -> BTreeMap<String, String> {
    body.pointer("/data")
        .and_then(Value::as_object)
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default()
}

/// Keys become file names under the volume dir: reject empties, path
/// separators, NULs, and the marker name itself (traversal/overwrite guard).
fn is_safe_key(key: &str) -> bool {
    !key.is_empty() && key != FINGERPRINT_FILE && !key.contains('/') && !key.contains('\0')
}

/// Remove one pod's whole per-pod dir, `volumes/` included; NotFound is
/// tolerated (the pod never materialized anything, or was already
/// cleaned). Live NFS mounts under the volume root are unmounted FIRST --
/// see [`cleanup_volumes_in`].
pub async fn cleanup_volumes(log_root: &Path, uid: &str) -> Result<(), String> {
    let mounts =
        std::fs::read_to_string(PROC_MOUNTS).map_err(|e| format!("read {PROC_MOUNTS}: {e}"))?;
    cleanup_volumes_in(Path::new("umount"), &mounts, log_root, uid).await
}

/// The real `/proc/self/mounts` path (factored out so tests can pass
/// synthetic mounts text instead of touching the host mount table).
const PROC_MOUNTS: &str = "/proc/self/mounts";

/// `symlink_metadata` gate for a path cleanup is about to `remove_dir_all`
/// (C4): absent is fine (`Ok(false)`), a plain directory is fine
/// (`Ok(true)`), anything else -- a symlink or a plain file where the
/// kubelet-owned tree should have a directory -- is an anomaly and is
/// REFUSED: never followed, never unlinked.
async fn removable_dir_state(dir: &Path) -> Result<bool, String> {
    match tokio::fs::symlink_metadata(dir).await {
        Ok(meta) if !meta.is_dir() => Err(format!(
            "refusing to remove {}: not a plain directory",
            dir.display()
        )),
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(format!("stat {}: {e}", dir.display())),
    }
}

/// `remove_dir_all` behind the [`removable_dir_state`] gate; a directory
/// that vanished between the two calls is tolerated (a concurrent sweep
/// or an earlier cycle got there first).
async fn remove_dir_all_if_present(dir: &Path) -> Result<(), String> {
    if !removable_dir_state(dir).await? {
        return Ok(());
    }
    match tokio::fs::remove_dir_all(dir).await {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("remove {}: {e}", dir.display())),
    }
}

/// The cleanup BODY with the mounts text injected (T-anyhome, Q36): the
/// `umount_bin` parameter is the prod PATH name (`umount`); tests inject
/// a fake script. Safety ordering is the whole point -- a live NFS mount
/// must be unmounted BEFORE `remove_dir_all`, because deleting through
/// the mount would destroy the SERVER-side files (anyhome memory). A
/// umount failure leaves the tree in place and returns `Err` (the cycle
/// retries).
///
/// Order (C4): uid check -> per-pod dir shape gate -> unmount under
/// `volumes/` -> remove `volumes/` -> remove the whole per-pod dir
/// `<log-root>/<uid>`, whose remaining content is only the pod's
/// date-shaped log dirs (T4.6). A refused uid or a malformed per-pod dir
/// fails before anything is unmounted or removed.
async fn cleanup_volumes_in(
    umount_bin: &Path,
    mounts_text: &str,
    log_root: &Path,
    uid: &str,
) -> Result<(), String> {
    if !is_safe_uid(uid) {
        return Err(format!("refusing to clean pod dir: rejected uid {uid:?}"));
    }
    let pod = pod_dir(log_root, uid);
    // Gate the per-pod dir FIRST: a symlinked `<log-root>/<uid>` would
    // otherwise be followed by the volume removal underneath it.
    removable_dir_state(&pod).await?;
    let root = volume_root(log_root, uid);
    nfs_mount::unmount_under(umount_bin, mounts_text, &root).await?;
    remove_dir_all_if_present(&root).await?;
    remove_dir_all_if_present(&pod).await?;
    Ok(())
}

// ---- pure CRI resolution --------------------------------------------------

/// Pure CRI mount resolution (S2): one [`Mount`] per `volumeMount` whose
/// name exists in the pod's volumes. configMap/secret volumes mount the
/// whole volume dir read-only (no per-key mounts, no subPath in v1);
/// emptyDir honors the declared `readOnly`. Unknown volume names are
/// skipped — the CRI create then fails and the cycle retries once
/// `ensure_volumes` converges.
pub fn resolve_mounts(
    spec: &ContainerSpec,
    volumes: &[VolumeSpec],
    uid: &str,
    log_root: &Path,
) -> Vec<Mount> {
    spec.volume_mounts
        .iter()
        .filter_map(|m| {
            let vol = volumes.iter().find(|v| v.name == m.name)?;
            let (host_path, readonly) = match &vol.kind {
                VolumeKind::EmptyDir => (empty_dir_path(log_root, uid, &vol.name), m.read_only),
                VolumeKind::ConfigMap { name } => {
                    (volume_dir(log_root, uid, "configMap", name), true)
                }
                VolumeKind::Secret { name } => (volume_dir(log_root, uid, "secret", name), true),
                // nfs honors BOTH flags (unlike configMap/secret, which
                // force true): the mount-level readOnly ORs with the
                // volume-source readOnly (T-anyhome, Q36).
                VolumeKind::Nfs { read_only, .. } => (
                    nfs_volume_path(log_root, uid, &vol.name),
                    m.read_only || *read_only,
                ),
            };
            Some(Mount {
                container_path: m.mount_path.clone(),
                host_path: host_path.to_string_lossy().into_owned(),
                readonly,
            })
        })
        .collect()
}

/// FNV-1a 64 over the sorted `key\0value\n` concatenation — deterministic
/// across runs (no `std::collections::hash_map::DefaultHasher`, whose seed
/// is randomized per process).
fn fingerprint(data: &BTreeMap<String, String>) -> u64 {
    let mut buf = String::new();
    for (k, v) in data {
        buf.push_str(k);
        buf.push('\0');
        buf.push_str(v);
        buf.push('\n');
    }
    fnv1a(buf.as_bytes())
}

fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in data {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

#[cfg(test)]
mod cleanup_tests;
#[cfg(test)]
mod tests;
