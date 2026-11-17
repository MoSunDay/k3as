//! Exec over the builtin engine (T4.12, Q50): rides the container's own
//! connection via its pump task — the engine refuses a second session
//! while a container runs.

use std::time::Duration;

use tokio::sync::oneshot;

use super::pump::Ctrl;
use super::registry::Registry;

/// `kubectl exec` surface: merged output + exit code, bounded by
/// `timeout` (the exec itself may outlive the caller's patience; its
/// reply channel is then simply dropped).
pub(crate) async fn exec(
    registry: &Registry,
    container_id: &str,
    cmd: Vec<String>,
    timeout: Duration,
) -> Result<(i32, String), String> {
    let Some(ctrl) = registry.ctrl(container_id) else {
        return Err(format!(
            "container {container_id} has no live engine (not running?)"
        ));
    };
    let (tx, rx) = oneshot::channel();
    ctrl.send(Ctrl::Exec { cmd, reply: tx })
        .await
        .map_err(|_| format!("container {container_id} pump is gone"))?;
    match tokio::time::timeout(timeout, rx).await {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => Err(format!("container {container_id} pump dropped the exec")),
        Err(_) => Err(format!("exec into {container_id} timed out")),
    }
}
