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

/// Remove one pod's whole volume root; NotFound is tolerated (the pod
/// never materialized a volume, or was already cleaned). Live NFS mounts
/// under the root are unmounted FIRST -- see [`cleanup_volumes_in`].
pub async fn cleanup_volumes(log_root: &Path, uid: &str) -> Result<(), String> {
    let mounts =
        std::fs::read_to_string(PROC_MOUNTS).map_err(|e| format!("read {PROC_MOUNTS}: {e}"))?;
    cleanup_volumes_in(Path::new("umount"), &mounts, log_root, uid).await
}

/// The real `/proc/self/mounts` path (factored out so tests can pass
/// synthetic mounts text instead of touching the host mount table).
const PROC_MOUNTS: &str = "/proc/self/mounts";

/// The cleanup BODY with the mounts text injected (T-anyhome, Q36): the
/// `umount_bin` parameter is the prod PATH name (`umount`); tests inject
/// a fake script. Safety ordering is the whole point -- a live NFS mount
/// must be unmounted BEFORE `remove_dir_all`, because deleting through
/// the mount would destroy the SERVER-side files (anyhome memory). A
/// umount failure leaves the tree in place and returns `Err` (the cycle
/// retries).
async fn cleanup_volumes_in(
    umount_bin: &Path,
    mounts_text: &str,
    log_root: &Path,
    uid: &str,
) -> Result<(), String> {
    let root = volume_root(log_root, uid);
    nfs_mount::unmount_under(umount_bin, mounts_text, &root).await?;
    match tokio::fs::remove_dir_all(&root).await {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("remove {}: {e}", root.display())),
    }
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
mod tests {
    use super::*;
    use crate::objects::pod_view;

    fn temp_root(tag: &str) -> PathBuf {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        std::env::temp_dir().join(format!("init-pro-volumes-{tag}-{}-{n}", std::process::id()))
    }

    fn pod_with_volumes(volumes: Value) -> PodView {
        pod_view(&serde_json::json!({
            "metadata": {"name": "p", "namespace": "default", "uid": "u1"},
            "spec": {"nodeName": "n1", "volumes": volumes, "containers": [
                {"name": "c", "image": "i", "volumeMounts": [
                    {"name": "cache", "mountPath": "/data"},
                    {"name": "cfg", "mountPath": "/etc/cm", "readOnly": true},
                    {"name": "unknown", "mountPath": "/x"},
                ]},
            ]},
        }))
        .unwrap()
    }

    #[test]
    fn path_helpers_layout() {
        let root = Path::new("/dd/pods");
        assert_eq!(volume_root(root, "u1"), Path::new("/dd/pods/u1/volumes"));
        assert_eq!(
            empty_dir_path(root, "u1", "cache"),
            Path::new("/dd/pods/u1/volumes/emptyDir/cache")
        );
        assert_eq!(
            config_map_path(root, "u1", "cm", "greeting"),
            Path::new("/dd/pods/u1/volumes/configMap/cm/greeting")
        );
        assert_eq!(
            secret_path(root, "u1", "s1", "token"),
            Path::new("/dd/pods/u1/volumes/secret/s1/token")
        );
    }

    #[test]
    fn resolve_mounts_maps_kinds_and_skips_unknown() {
        let pod = pod_with_volumes(serde_json::json!([
            {"name": "cache", "emptyDir": {}},
            {"name": "cfg", "configMap": {"name": "cm1"}},
        ]));
        let mounts = resolve_mounts(
            &pod.containers[0],
            &pod.volumes,
            &pod.uid,
            Path::new("/dd/pods"),
        );
        assert_eq!(
            mounts,
            vec![
                Mount {
                    container_path: "/data".into(),
                    host_path: "/dd/pods/u1/volumes/emptyDir/cache".into(),
                    readonly: false,
                },
                Mount {
                    container_path: "/etc/cm".into(),
                    host_path: "/dd/pods/u1/volumes/configMap/cm1".into(),
                    readonly: true,
                },
            ],
            "emptyDir keeps the declared readOnly; configMap forces true; unknown name skipped"
        );
        // Secret mounts are read-only too.
        let pod = pod_with_volumes(serde_json::json!([
            {"name": "cfg", "secret": {"secretName": "s1"}},
        ]));
        let mounts = resolve_mounts(
            &pod.containers[0],
            &pod.volumes,
            &pod.uid,
            Path::new("/dd/pods"),
        );
        assert_eq!(mounts[0].host_path, "/dd/pods/u1/volumes/secret/s1");
        assert!(mounts[0].readonly);
    }

    #[test]
    fn resolve_mounts_nfs_readonly_or_folding() {
        // Source readOnly=false: the volumeMount flag decides.
        let pod = pod_with_volumes(serde_json::json!([
            {"name": "cache", "nfs": {"server": "anyhome:2049", "path": "/exports/u1"}},
        ]));
        let mounts = resolve_mounts(
            &pod.containers[0],
            &pod.volumes,
            &pod.uid,
            Path::new("/dd/pods"),
        );
        assert_eq!(
            mounts[0].host_path, "/dd/pods/u1/volumes/nfs/cache",
            "nfs mounts the volume dir, not the export string"
        );
        assert!(!mounts[0].readonly, "rw source + rw mount -> rw");
        // Source readOnly=true forces read-only even for a rw volumeMount...
        let pod = pod_with_volumes(serde_json::json!([
            {"name": "cache", "nfs": {"server": "anyhome", "path": "/r", "readOnly": true}},
        ]));
        let mounts = resolve_mounts(
            &pod.containers[0],
            &pod.volumes,
            &pod.uid,
            Path::new("/dd/pods"),
        );
        assert!(mounts[0].readonly, "source readOnly participates (Q36)");
        // ...and the mount-level flag alone still forces it.
        let pod = pod_with_volumes(serde_json::json!([
            {"name": "cfg", "nfs": {"server": "anyhome", "path": "/r"}},
        ]));
        let mut c = pod.containers[0].clone();
        c.volume_mounts[0].read_only = true;
        let mounts = resolve_mounts(&c, &pod.volumes, &pod.uid, Path::new("/dd/pods"));
        assert!(mounts[0].readonly, "mount readOnly still wins on its own");
    }

    /// A fake `umount` binary (executable script); the log path is baked
    /// in so PATH/env is never touched (parallel-test race safety).
    fn fake_umount(dir: &Path, tag: &str, exit: i32) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let log = dir.join(format!("{tag}.log"));
        let script = dir.join(format!("fake-umount-{tag}.sh"));
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" >> {}\necho 'umount: failed' >&2\nexit {exit}\n",
                log.display()
            ),
        )
        .unwrap();
        let mut perms = std::fs::metadata(&script).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script, perms).unwrap();
        script
    }

    #[tokio::test]
    async fn cleanup_umount_failure_preserves_tree_and_returns_err() {
        let root = temp_root("umount-fail");
        let nfs_dir = nfs_volume_path(&root, "u1", "x");
        tokio::fs::create_dir_all(&nfs_dir).await.unwrap();
        tokio::fs::write(nfs_dir.join("canary"), "server-side data")
            .await
            .unwrap();
        let mounts = format!(
            "anyhome:/exports /run/nfs nfs3 rw 0 0\nanyhome:/exports {} nfs3 rw 0 0\n",
            nfs_dir.display()
        );
        let umount = fake_umount(&root, "fail", 16);
        let err = cleanup_volumes_in(&umount, &mounts, &root, "u1")
            .await
            .unwrap_err();
        assert!(err.contains("umount: failed"), "stderr surfaced: {err}");
        assert!(
            volume_root(&root, "u1").exists()
                && tokio::fs::try_exists(nfs_dir.join("canary")).await.unwrap(),
            "umount-before-rm: the tree (and canary) SURVIVE a failed umount -- \
             deleting through a live NFS mount would destroy server-side memory (Q36)"
        );
    }

    #[tokio::test]
    async fn cleanup_successful_umount_then_removes_tree() {
        let root = temp_root("umount-ok");
        let nfs_dir = nfs_volume_path(&root, "u1", "x");
        tokio::fs::create_dir_all(&nfs_dir).await.unwrap();
        tokio::fs::write(nfs_dir.join("canary"), "server-side data")
            .await
            .unwrap();
        let mounts = format!("anyhome:/exports {} nfs3 rw 0 0\n", nfs_dir.display());
        let umount = fake_umount(&root, "ok", 0);
        cleanup_volumes_in(&umount, &mounts, &root, "u1")
            .await
            .unwrap();
        assert!(
            !volume_root(&root, "u1").exists(),
            "unmounted cleanly, tree removed"
        );
        // NotFound tolerance still holds with the mounts text present.
        cleanup_volumes_in(&umount, &mounts, &root, "u1")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn cleanup_without_mounts_still_removes_tree() {
        let root = temp_root("plain");
        let dir = volume_dir(&root, "u1", "emptyDir", "cache");
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let umount = fake_umount(&root, "none", 0);
        cleanup_volumes_in(&umount, "nodev /proc procfs rw 0 0", &root, "u1")
            .await
            .unwrap();
        assert!(
            !volume_root(&root, "u1").exists(),
            "no nfs mounts under root -> plain rmdir-all"
        );
    }

    #[tokio::test]
    async fn empty_dir_created_and_cleanup_tolerates_not_found() {
        let root = temp_root("ed");
        let pod = pod_with_volumes(serde_json::json!([{"name": "cache", "emptyDir": {}}]));
        let wanted = BTreeMap::from([(pod.key.clone(), pod)]);
        // The emptyDir branch never touches the client (port 9: discard).
        let client = HttpJson::parse_url("http://127.0.0.1:9").unwrap();
        ensure_volumes(&client, &wanted, &root).await.unwrap();
        assert!(empty_dir_path(&root, "u1", "cache").is_dir());
        cleanup_volumes(&root, "u1").await.unwrap();
        assert!(!volume_root(&root, "u1").exists(), "volume root removed");
        cleanup_volumes(&root, "u1").await.unwrap(); // NotFound tolerated
    }

    #[tokio::test]
    async fn fingerprint_gates_rewrites() {
        let root = temp_root("fp");
        let dir = volume_dir(&root, "u1", "configMap", "cm");
        let data = BTreeMap::from([
            ("greeting".to_string(), "hello".to_string()),
            ("mode".to_string(), "prod".to_string()),
        ]);
        assert!(apply_if_changed(&dir, &data).await.unwrap(), "first write");
        assert_eq!(
            tokio::fs::read_to_string(config_map_path(&root, "u1", "cm", "greeting"))
                .await
                .unwrap(),
            "hello"
        );
        // Same content -> no rewrite (proven by tamper persistence: the
        // marker short-circuits before any file I/O).
        let greeting = config_map_path(&root, "u1", "cm", "greeting");
        tokio::fs::write(&greeting, "tampered").await.unwrap();
        assert!(!apply_if_changed(&dir, &data).await.unwrap(), "unchanged");
        assert_eq!(
            tokio::fs::read_to_string(&greeting).await.unwrap(),
            "tampered"
        );
        // Changed content -> files + marker rewritten.
        let changed = BTreeMap::from([
            ("greeting".to_string(), "bye".to_string()),
            ("mode".to_string(), "prod".to_string()),
        ]);
        assert!(apply_if_changed(&dir, &changed).await.unwrap(), "changed");
        assert_eq!(tokio::fs::read_to_string(&greeting).await.unwrap(), "bye");
        // And the marker now matches, so the next pass is a no-op again.
        assert!(!apply_if_changed(&dir, &changed).await.unwrap());
    }

    #[tokio::test]
    async fn unsafe_keys_skipped_and_empty_data_writes_marker_only() {
        let root = temp_root("keys");
        let dir = volume_dir(&root, "u1", "configMap", "cm");
        let data = BTreeMap::from([
            ("ok".to_string(), "1".to_string()),
            ("../escape".to_string(), "x".to_string()),
            (".fingerprint".to_string(), "spoof".to_string()),
        ]);
        apply_if_changed(&dir, &data).await.unwrap();
        let mut names: Vec<String> = Vec::new();
        let mut entries = tokio::fs::read_dir(&dir).await.unwrap();
        while let Some(e) = entries.next_entry().await.unwrap() {
            names.push(e.file_name().to_string_lossy().into_owned());
        }
        names.sort();
        assert_eq!(names, vec![".fingerprint".to_string(), "ok".to_string()]);
        let marker = tokio::fs::read_to_string(dir.join(FINGERPRINT_FILE))
            .await
            .unwrap();
        assert_eq!(
            marker,
            format!(
                "{:016x}",
                fingerprint(&BTreeMap::from([("ok".to_string(), "1".to_string())]))
            )
        );
        // Empty data -> marker present, no key files.
        let empty = volume_dir(&root, "u1", "configMap", "empty");
        apply_if_changed(&empty, &BTreeMap::new()).await.unwrap();
        let mut entries = tokio::fs::read_dir(&empty).await.unwrap();
        let mut count = 0;
        while entries.next_entry().await.unwrap().is_some() {
            count += 1;
        }
        assert_eq!(count, 1, "marker only");
    }
}
