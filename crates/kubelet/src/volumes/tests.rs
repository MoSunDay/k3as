//! volumes.rs tests, part 1 (T4.2 Scope B): path layout, pure CRI mount
//! resolution and the fingerprint-gated configMap/secret materialization.
//! The cleanup ordering + C4 per-pod-dir release cases live in
//! `cleanup_tests.rs` (file size limit).
use super::*;
use crate::objects::pod_view;

pub(super) fn temp_root(tag: &str) -> PathBuf {
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
    assert_eq!(pod_dir(root, "u1"), Path::new("/dd/pods/u1"));
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

#[tokio::test]
async fn empty_dir_created_and_cleanup_tolerates_not_found() {
    let root = temp_root("ed");
    let pod = pod_with_volumes(serde_json::json!([{"name": "cache", "emptyDir": {}}]));
    let wanted = BTreeMap::from([(pod.key.clone(), pod)]);
    // The emptyDir branch never touches the client (port 9: discard).
    let client = HttpJson::parse_url("http://127.0.0.1:9").unwrap();
    ensure_volumes(&client, &wanted, &root).await.unwrap();
    assert!(empty_dir_path(&root, "u1", "cache").is_dir());
    assert!(pod_dir(&root, "u1").is_dir());
    cleanup_volumes(&root, "u1").await.unwrap();
    assert!(!volume_root(&root, "u1").exists(), "volume root removed");
    assert!(!pod_dir(&root, "u1").exists(), "per-pod dir removed (C4)");
    cleanup_volumes(&root, "u1").await.unwrap(); // NotFound tolerated
    let _ = std::fs::remove_dir_all(&root);
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
