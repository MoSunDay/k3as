//! Overlayfs capability probe (T4.13 M3, Q53): can THIS process mount a
//! real `overlay` (lowerdir/upperdir/workdir) here? Root privileges plus
//! kernel support answer yes and the image store serves per-container
//! overlayfs views; anything else keeps the whiteout-aware fused-copy
//! fallback. Pure best-effort: the probe root is created, mounted,
//! detached and removed again.

use std::path::Path;

/// Mount-and-release probe under `root` (a scratch dir the probe owns
/// completely). Never touches anything outside it.
pub(crate) fn overlay_supported(root: &Path) -> bool {
    if !nix::unistd::geteuid().is_root() {
        return false;
    }
    let lower = root.join("lower");
    let upper = root.join("upper");
    let work = root.join("work");
    let mnt = root.join("mnt");
    let prepared = [&lower, &upper, &work, &mnt]
        .iter()
        .all(|d| std::fs::create_dir_all(d).is_ok())
        && std::fs::write(lower.join("probe"), b"q53").is_ok();
    if !prepared {
        return false;
    }
    let data = format!(
        "lowerdir={},upperdir={},workdir={}",
        lower.display(),
        upper.display(),
        work.display()
    );
    let mounted = nix::mount::mount(
        Some("overlay"),
        &mnt,
        Some("overlay"),
        nix::mount::MsFlags::empty(),
        Some(data.as_str()),
    )
    .is_ok();
    // Detach before removing: the mountpoint lives under `root`.
    let _ = nix::mount::umount2(&mnt, nix::mount::MntFlags::MNT_DETACH);
    let _ = std::fs::remove_dir_all(root);
    mounted
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_a_bool_without_leaving_the_probe_behind() {
        let root = std::env::temp_dir().join(format!(
            "init-pro-overlay-probe-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let ok = overlay_supported(&root);
        // Root: expected true on a normal Linux node; unprivileged runs
        // answer false. Either way the scratch dir must be gone.
        assert!(!root.exists(), "probe scratch left behind (ok={ok})");
    }
}
