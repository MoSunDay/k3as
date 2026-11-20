//! Module-root unit tests for the GPU plane (TODO **T7.2**, Q61):
//! ledger path, the backend guard matrix, prepare's env-table error
//! channel and teardown completion. Split into this file to keep
//! `gpu.rs` inside the 400-line new-file budget.

use std::collections::BTreeMap;

use serde_json::json;

use crate::objects::{pod_view, PodView};
use crate::sync::{self, Snapshot};

use super::*;

fn gpu_pod(annotations: &[(&str, &str)], count: &str) -> PodView {
    pod_with_request(annotations, Some(count))
}

/// A pod with NO GPU resource key and no rootfs annotation — the guard
/// must ignore it entirely (plain pods never touch the GPU plane).
fn plain_pod() -> PodView {
    pod_with_request(&[], None)
}

fn pod_with_request(annotations: &[(&str, &str)], count: Option<&str>) -> PodView {
    let mut ann = serde_json::Map::new();
    for (k, v) in annotations {
        ann.insert(k.to_string(), json!(v));
    }
    let mut container = json!({"name": "c"});
    if let Some(n) = count {
        container["resources"] = json!({"requests": {"nvidia.com/gpu": n}});
    }
    pod_view(&json!({
        "metadata": {
            "name": "p", "namespace": "default", "uid": "uid-1",
            "annotations": ann,
        },
        "spec": {"nodeName": "n", "containers": [container]}
    }))
    .unwrap()
}

fn view_in_map(view: &PodView) -> BTreeMap<String, PodView> {
    BTreeMap::from([(view.key.clone(), view.clone())])
}

#[test]
fn gpu_t72_ledger_path_under_engine() {
    assert_eq!(
        ledger_path(Path::new("/data")),
        Path::new("/data/engine/gpu-ledger.json")
    );
}

/// GPU1 wiring of the ONE guard (Q60/Q61): plain pods pass everywhere;
/// GPU/host-direct pods demand the builtin engine; GPU pods additionally
/// demand capability and the literal `/` rootfs annotation.
#[test]
fn gpu_t72_backend_guard_matrix() {
    let plain = plain_pod();
    assert_eq!(sync::gpu_backend_guard(&plain, false, false), None);

    let host_direct = gpu_pod(&[("init-pro.io/rootfs", "/")], "0");
    assert!(sync::gpu_backend_guard(&host_direct, false, false)
        .unwrap()
        .contains("require the builtin engine"));

    let gpu = gpu_pod(&[("init-pro.io/rootfs", "/")], "1");
    assert!(sync::gpu_backend_guard(&gpu, false, true)
        .unwrap()
        .contains("require the builtin engine"));
    assert!(sync::gpu_backend_guard(&gpu, true, false)
        .unwrap()
        .contains("not GPU-capable"));
    assert_eq!(sync::gpu_backend_guard(&gpu, true, true), None);

    let unannotated = gpu_pod(&[], "1");
    let reason = sync::gpu_backend_guard(&unannotated, true, true).unwrap();
    assert!(reason.contains("init-pro.io/rootfs"), "{reason}");
}

/// prepare() folds guard failures into the env table (never clobbering
/// an existing error) so the stock event pass reports them; a valid GPU
/// pod on a capable node allocates deterministically (first-free).
#[test]
fn gpu_t72_prepare_blocks_and_allocates() {
    let dir = std::env::temp_dir().join(format!("gpu-prep-{}", std::process::id()));
    let mut rt = GpuRuntime::open(
        &dir,
        GpuInventorySource::Fake {
            resource: "nvidia.com/gpu".to_string(),
            count: 1,
        },
    );
    let cfg = KubeletConfig::new("http://127.0.0.1:1", "n", dir.clone());
    let mut cfg = cfg;
    cfg.engine_builtin = true;
    cfg.builtin_rootfs = crate::engine::RootfsMode::Host;

    // Invalid: GPU key with a fractional request -> blocking error.
    let mut table = crate::env_fetch::EnvTable::new();
    let frac = gpu_pod(&[("init-pro.io/rootfs", "/")], "500m");
    let wants_empty = pod_view_gpu_wants(&frac).is_empty();
    assert!(wants_empty);
    let prepared = prepare(&mut rt, &cfg, &view_in_map(&frac), &mut table, 100);
    assert!(prepared.devices.is_empty());
    let err = table
        .get("default/p")
        .unwrap()
        .get("c")
        .unwrap()
        .error
        .clone();
    assert!(err.unwrap().contains("whole-device"), "error channel");

    // Valid: one device universe, allocated first-free and injected into
    // the prepared map under (pod key, container).
    let mut table = crate::env_fetch::EnvTable::new();
    let good = gpu_pod(&[("init-pro.io/rootfs", "/")], "1");
    let prepared = prepare(&mut rt, &cfg, &view_in_map(&good), &mut table, 101);
    assert_eq!(
        prepared.devices_for("default/p", "c"),
        Some(("nvidia.com/gpu".to_string().as_str(), &[0u64][..]))
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// GPU4 teardown completion: released only when the uid left the desired
/// set AND the observed snapshot (one cycle after the CRI teardown).
#[test]
fn gpu_t72_completed_teardowns_need_desired_and_observed_absence() {
    let live = gpu_pod(&[], "0");
    let mut desired = view_in_map(&live);
    let snap = Snapshot::default();
    assert!(sync::completed_gpu_teardowns(&["uid-1".to_string()], &desired, &snap).is_empty());

    desired.remove("default/p");
    let gone = sync::completed_gpu_teardowns(&["uid-1".to_string()], &desired, &snap);
    assert_eq!(gone, vec!["uid-1".to_string()]);

    // A sandbox still carrying the uid holds the release one more cycle.
    let mut snap = Snapshot::default();
    snap.sandboxes.push(crate::cri_backend::SandboxView {
        id: "sb".into(),
        state: "SANDBOX_READY".into(),
        labels: BTreeMap::new(),
        name: "p".into(),
        namespace: "default".into(),
        uid: "uid-1".into(),
        ip: None,
    });
    assert!(sync::completed_gpu_teardowns(&["uid-1".to_string()], &desired, &snap).is_empty());
}
