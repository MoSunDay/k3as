//! C3 (bug sweep) tests for `super` -- the disk-pressure image GC pass.
//! Split out of `pressure.rs` at the 400-line rule. Each case drives
//! `pass_at` with a synthetic `(available, total)` reading and a clock one
//! hour ahead, so no case needs a full disk or a real sleep.

use super::super::tests::{sandbox_entry, temp_data_dir};
use super::*;
use crate::engine::image::PulledImage;

const GIB: u64 = 1024 * 1024 * 1024;

fn backend(tag: &str) -> (EngineBackend, std::path::PathBuf) {
    let dir = temp_data_dir(tag);
    std::fs::create_dir_all(&dir).unwrap();
    let backend = EngineBackend::new(
        &dir,
        "registry.local/pause:0",
        "192.0.2.7",
        "registry.local:5000",
    )
    .unwrap();
    (backend, dir)
}

fn descriptor(kind: &str, salt: &str) -> crate::engine::image::Descriptor {
    crate::engine::image::Descriptor {
        media_type: "application/vnd.oci.image.layer.v1.tar".into(),
        digest: format!("sha256:{kind}{salt}"),
        size: 5,
        platform: None,
    }
}

fn image(reference: &str, salt: &str) -> PulledImage {
    PulledImage {
        reference: reference.to_string(),
        digest: format!("sha256:m{salt}"),
        config: descriptor("c", salt),
        layers: vec![descriptor("l", salt)],
    }
}

/// Write a blob and backdate it.
fn blob(store: &crate::engine::image::ImageStore, digest: &str, age: std::time::Duration) {
    let path = store.blob_path(digest);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, b"payload").unwrap();
    set_age(&path, age);
}

/// Backdate a path (file or dir) so the grace filter sees it as old.
fn set_age(path: &Path, age: std::time::Duration) {
    let file = std::fs::File::open(path).unwrap();
    file.set_modified(SystemTime::now() - age).unwrap();
}

/// Backdate one pull record's ref file.
fn age_ref(store: &crate::engine::image::ImageStore, reference: &str, age: std::time::Duration) {
    set_age(&ref_path(store, reference), age);
}

/// The pass runs on the real clock: entries are backdated past the grace
/// age instead (a skewed clock would make even a just-written blob read as
/// aged).
fn now() -> SystemTime {
    SystemTime::now()
}

fn healthy_reading() -> (u64, u64) {
    (50 * GIB, 100 * GIB)
}

fn critical_reading() -> (u64, u64) {
    (0, 100 * GIB)
}

fn references(backend: &EngineBackend) -> Vec<String> {
    backend
        .store
        .list_refs()
        .into_iter()
        .map(|r| r.reference)
        .collect()
}

#[test]
fn healthy_root_touches_nothing() {
    let (backend, dir) = backend("gc-healthy");
    let digest = format!("sha256:{}", "a".repeat(64));
    blob(&backend.store, &digest, std::time::Duration::from_secs(0));
    let (available, total) = healthy_reading();
    let stats = pass_at(&backend, &[], available, total, now());
    assert_eq!(stats.pressure, DiskPressure::Healthy);
    assert_eq!(
        (stats.entries_removed, stats.refs_removed, stats.bytes_freed),
        (0, 0, 0)
    );
    assert!(backend.store.has_blob(&digest));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn pressure_sweeps_only_aged_orphans() {
    let (backend, dir) = backend("gc-pressure");
    let store = &backend.store;
    let img = image("registry.local:5000/lib/keep:1", "1");
    store.put_ref(&img).unwrap();
    for digest in [&img.digest, &img.config.digest, &img.layers[0].digest] {
        blob(store, digest, std::time::Duration::from_secs(9_000));
    }
    std::fs::create_dir_all(store.layer_dir(&img.layers[0].digest)).unwrap();
    set_age(
        &store.layer_dir(&img.layers[0].digest),
        std::time::Duration::from_secs(9_000),
    );

    let aged = format!("sha256:{}", "b".repeat(64));
    blob(store, &aged, std::time::Duration::from_secs(9_000));
    std::fs::create_dir_all(store.layer_dir(&aged)).unwrap();
    set_age(
        &store.layer_dir(&aged),
        std::time::Duration::from_secs(9_000),
    );
    std::fs::create_dir_all(store.merged_dir(&aged)).unwrap();
    set_age(
        &store.merged_dir(&aged),
        std::time::Duration::from_secs(9_000),
    );
    // A young orphan = an in-flight pull's blob; it must survive.
    let young = format!("sha256:{}", "c".repeat(64));
    blob(store, &young, std::time::Duration::from_secs(0));

    let stats = pass_at(&backend, &[], 5 * GIB, 100 * GIB, now());
    assert_eq!(stats.pressure, DiskPressure::Pressure);
    assert_eq!(stats.refs_removed, 0, "Pressure keeps every pull record");
    assert_eq!(stats.entries_removed, 3, "blob + layer dir + merged dir");
    assert!(stats.bytes_freed > 0);
    assert!(!store.has_blob(&aged));
    assert!(!store.layer_dir(&aged).exists());
    assert!(!store.merged_dir(&aged).exists());
    assert!(
        store.has_blob(&young),
        "a young orphan is an in-flight pull"
    );
    for digest in [&img.digest, &img.config.digest, &img.layers[0].digest] {
        assert!(store.has_blob(digest), "a ref still owns {digest}");
    }
    assert!(store.layer_dir(&img.layers[0].digest).is_dir());
    assert_eq!(store.list_refs().len(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn critical_drops_oldest_unused_refs_and_keeps_in_use() {
    let (backend, dir) = backend("gc-critical");
    let store = &backend.store;
    let mut images = Vec::new();
    for (name, salt) in [("old", "1"), ("mid", "2"), ("fresh", "3"), ("used", "4")] {
        let img = image(&format!("registry.local:5000/lib/{name}:1"), salt);
        store.put_ref(&img).unwrap();
        blob(
            store,
            &img.layers[0].digest,
            std::time::Duration::from_secs(9_000),
        );
        age_ref(store, &img.reference, std::time::Duration::from_secs(9_000));
        images.push(img);
    }
    // `fresh` was pulled a minute ago: the grace age keeps it.
    age_ref(
        store,
        "registry.local:5000/lib/fresh:1",
        std::time::Duration::from_secs(60),
    );

    // Critical: the two oldest UNUSED refs go; `used` is protected by the
    // kubelet's desired pod set, `fresh` by the grace age.
    let in_use = vec!["registry.local:5000/lib/used:1".to_string()];
    let (available, total) = critical_reading();
    let stats = pass_at(&backend, &in_use, available, total, now());
    assert_eq!(stats.pressure, DiskPressure::Critical);
    assert_eq!(stats.refs_removed, 2, "oldest first, unused only");
    let left = references(&backend);
    assert!(left.contains(&"registry.local:5000/lib/used:1".to_string()));
    assert!(left.contains(&"registry.local:5000/lib/fresh:1".to_string()));
    assert!(!left.contains(&"registry.local:5000/lib/old:1".to_string()));
    assert!(!left.contains(&"registry.local:5000/lib/mid:1".to_string()));
    // The dropped refs' unique layers became orphans and went too.
    assert!(!store.has_blob(&images[0].layers[0].digest));
    assert!(!store.has_blob(&images[1].layers[0].digest));
    assert!(store.has_blob(&images[2].layers[0].digest));
    assert!(store.has_blob(&images[3].layers[0].digest));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn critical_never_drops_the_pause_image_or_a_live_container_image() {
    let (backend, dir) = backend("gc-live");
    let store = &backend.store;
    let pause = image("registry.local/pause:0", "p");
    let live = image("registry.local:5000/lib/live:1", "l");
    let free = image("registry.local:5000/lib/free:1", "f");
    for img in [&pause, &live, &free] {
        store.put_ref(img).unwrap();
        age_ref(store, &img.reference, std::time::Duration::from_secs(9_000));
    }
    // A running container's registry entry protects `live`.
    let mut entry = sandbox_entry(&dir, "c1", "CONTAINER_RUNNING");
    entry.image = live.reference.clone();
    backend.registry.insert(entry);

    let (available, total) = critical_reading();
    let stats = pass_at(&backend, &[], available, total, now());
    let left = references(&backend);
    assert!(
        left.contains(&pause.reference),
        "the pause image is never a victim"
    );
    assert!(
        left.contains(&live.reference),
        "a live container holds its layers"
    );
    assert!(!left.contains(&free.reference));
    assert_eq!(stats.refs_removed, 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn critical_leaves_shared_layers_to_the_surviving_ref() {
    let (backend, dir) = backend("gc-shared");
    let store = &backend.store;
    let gone = image("registry.local:5000/lib/gone:1", "g");
    let mut kept = image("registry.local:5000/lib/kept:1", "k");
    kept.layers = vec![descriptor("l", "g")];
    for img in [&gone, &kept] {
        store.put_ref(img).unwrap();
        age_ref(store, &img.reference, std::time::Duration::from_secs(9_000));
    }
    let shared = &gone.layers[0].digest;
    blob(store, shared, std::time::Duration::from_secs(9_000));
    std::fs::create_dir_all(store.layer_dir(shared)).unwrap();
    set_age(
        &store.layer_dir(shared),
        std::time::Duration::from_secs(9_000),
    );

    let in_use = vec!["registry.local:5000/lib/kept:1".to_string()];
    let (available, total) = critical_reading();
    let stats = pass_at(&backend, &in_use, available, total, now());
    assert_eq!(stats.refs_removed, 1, "only `gone` is dropped");
    assert!(store.has_blob(shared), "dedupe survives the pressure pass");
    assert!(store.layer_dir(shared).is_dir());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn measure_reads_the_real_filesystem() {
    let (backend, dir) = backend("gc-measure");
    let (available, total) = measure(backend.store.root());
    assert!(total > 0, "the temp dir lives on a filesystem");
    assert!(available > 0);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn measure_maps_a_missing_root_to_healthy() {
    let (available, total) = measure(Path::new("/nonexistent-init-pro-c3/root"));
    assert_eq!((available, total), (0, 0));
    assert_eq!(
        disk::classify(available, total),
        DiskPressure::Healthy,
        "an unmeasurable root never triggers a sweep"
    );
}
