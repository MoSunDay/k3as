//! Log path computation (T4.12, Q50): the engine appends CRI text lines
//! to `<log_directory>/<log_path>` — the exact file the kubelet's log
//! reader serves — so this stays in one place for both sides.

use std::path::{Path, PathBuf};

/// The container's CRI log file. `log_directory` comes from the sandbox
/// config (dated dir, T4.6), `log_path` is container-relative.
pub(crate) fn log_file(log_directory: &str, log_path: &str) -> PathBuf {
    PathBuf::from(log_directory).join(log_path)
}

/// Parents are created before start (the engine's LogWriter also creates
/// them; doing it here keeps `exec.rs`'s pre-runp mkdir parity).
pub(crate) fn ensure_parents(file: &Path) -> std::io::Result<()> {
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joins_the_cri_pair() {
        let f = log_file("/var/log/pods/uid/2026-11-17", "web.0.log");
        assert_eq!(f, PathBuf::from("/var/log/pods/uid/2026-11-17/web.0.log"));
    }
}
