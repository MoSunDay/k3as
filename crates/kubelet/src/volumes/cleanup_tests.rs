//! volumes.rs tests, part 2 (T-anyhome Q36 / C4): the umount-before-rm
//! cleanup ordering, and the per-pod dir release -- `<log_root>/<uid>`
//! goes away with `volumes/`, behind the uid shape guard and the
//! plain-directory guard.
use super::tests::temp_root;
use super::*;

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
async fn cleanup_removes_the_whole_pod_dir_but_only_that_pod() {
    let root = temp_root("pod-dir");
    // u1: a volume tree plus two date-shaped log dirs (T4.6 layout).
    tokio::fs::create_dir_all(volume_dir(&root, "u1", "emptyDir", "cache"))
        .await
        .unwrap();
    for date in ["2026-11-20", "2026-11-25"] {
        let dir = pod_dir(&root, "u1").join(date);
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(dir.join("web.0.log"), "line\n")
            .await
            .unwrap();
    }
    // u2: a sibling pod that must survive u1's cleanup untouched.
    let sibling = pod_dir(&root, "u2").join("2026-11-26").join("web.0.log");
    tokio::fs::create_dir_all(sibling.parent().unwrap())
        .await
        .unwrap();
    tokio::fs::write(&sibling, "live\n").await.unwrap();

    let umount = fake_umount(&root, "pod-dir", 0);
    cleanup_volumes_in(&umount, "nodev /proc procfs rw 0 0", &root, "u1")
        .await
        .unwrap();
    assert!(!volume_root(&root, "u1").exists(), "volume root removed");
    assert!(
        !pod_dir(&root, "u1").exists(),
        "the whole per-pod dir (date logs included) is released (C4)"
    );
    assert!(root.is_dir(), "the log root itself survives");
    assert_eq!(
        std::fs::read_to_string(&sibling).unwrap(),
        "live\n",
        "a sibling pod's dir is untouched"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn cleanup_of_a_log_only_pod_dir_removes_it_and_tolerates_absence() {
    let root = temp_root("log-only");
    let log = pod_dir(&root, "u1").join("2026-11-20").join("web.0.log");
    tokio::fs::create_dir_all(log.parent().unwrap())
        .await
        .unwrap();
    tokio::fs::write(&log, "x").await.unwrap();
    let umount = fake_umount(&root, "log-only", 0);
    let mounts = "nodev /proc procfs rw 0 0";
    cleanup_volumes_in(&umount, mounts, &root, "u1")
        .await
        .unwrap();
    assert!(
        !pod_dir(&root, "u1").exists(),
        "a pod that only ever had logs is still released"
    );
    cleanup_volumes_in(&umount, mounts, &root, "u1")
        .await
        .unwrap(); // NotFound tolerated
    cleanup_volumes_in(&umount, mounts, &root, "never-existed")
        .await
        .unwrap();
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn uid_shape_is_a_bare_path_component() {
    for ok in ["u1", "A.B_c-9", "f47ac10b-58cc-4372-a567-0e02b2c3d479"] {
        assert!(is_safe_uid(ok), "{ok:?} must be accepted");
    }
    assert!(is_safe_uid(&"a".repeat(MAX_UID_LEN)));
    let long = "a".repeat(MAX_UID_LEN + 1);
    for bad in [
        "",
        ".",
        "..",
        "../x",
        "/x",
        "x/",
        "u1/volumes",
        "no-uid-default/pod",
        "a\\b",
        "a b",
        "a:b",
        "a\0b",
        long.as_str(),
    ] {
        assert!(!is_safe_uid(bad), "{bad:?} must be refused");
    }
}

#[tokio::test]
async fn cleanup_refuses_unsafe_uids_before_touching_the_filesystem() {
    // `base` is the disposable sandbox: a broken `..` guard could at
    // most delete `base` itself, never the real temp dir.
    let base = temp_root("uid");
    let root = base.join("root");
    tokio::fs::create_dir_all(volume_dir(&root, "u1", "emptyDir", "cache"))
        .await
        .unwrap();
    let canary = base.join("canary");
    tokio::fs::create_dir_all(&canary).await.unwrap();
    tokio::fs::write(canary.join("keep"), "x").await.unwrap();

    // Never invoked: a refused uid must fail before the umount step.
    let umount = Path::new("/nonexistent/init-pro-umount");
    for uid in [
        "",
        ".",
        "..",
        "../canary",
        "/etc",
        "u1/..",
        "u1/volumes",
        "a\\b",
        "nul\0",
        "no-uid-default/pod",
    ] {
        let err = cleanup_volumes_in(umount, "", &root, uid)
            .await
            .unwrap_err();
        assert!(err.contains("rejected uid"), "{uid:?}: {err}");
    }
    assert!(
        volume_root(&root, "u1").is_dir(),
        "a refused uid leaves the real pod's volumes alone"
    );
    assert!(canary.join("keep").is_file(), "the `..` canary survives");
    let _ = std::fs::remove_dir_all(&base);
}

#[tokio::test]
async fn cleanup_refuses_a_pod_dir_that_is_not_a_plain_directory() {
    let base = temp_root("symlink");
    let target = base.join("elsewhere");
    tokio::fs::create_dir_all(target.join("volumes"))
        .await
        .unwrap();
    tokio::fs::write(target.join("volumes").join("keep"), "x")
        .await
        .unwrap();
    let root = base.join("root");
    tokio::fs::create_dir_all(&root).await.unwrap();
    std::os::unix::fs::symlink(&target, pod_dir(&root, "u1")).unwrap();

    let umount = fake_umount(&root, "symlink", 0);
    let err = cleanup_volumes_in(&umount, "", &root, "u1")
        .await
        .unwrap_err();
    assert!(err.contains("not a plain directory"), "{err}");
    assert!(
        tokio::fs::try_exists(target.join("volumes").join("keep"))
            .await
            .unwrap(),
        "the link target is never followed or emptied"
    );
    assert!(
        !root.join("symlink.log").exists(),
        "the refusal happens BEFORE the umount step"
    );
    let _ = std::fs::remove_dir_all(&base);
}
