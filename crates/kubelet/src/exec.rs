//! Execution half of the reconcile core (TODO **T4.2**): applies the ordered
//! [`Action`] list from [`crate::sync::plan`] to a [`CriBackend`] —
//! sequentially, because CRI op ordering matters and each crictl op costs
//! ~20ms (Q26). Teardown ops tolerate "already gone" errors so cycles are
//! idempotent; every action yields an outcome so callers report, never panic.

use crate::cri_backend::CriBackend;
use crate::sync::{Action, Snapshot};

/// Apply actions sequentially (CRI op ordering matters; ~20ms each, Q26).
/// Returns one outcome per action so callers can report without panicking.
pub async fn execute(
    cri: &dyn CriBackend,
    actions: Vec<Action>,
) -> Vec<(Action, Result<(), String>)> {
    let mut out = Vec::with_capacity(actions.len());
    for action in actions {
        let res = run_one(cri, &action).await;
        out.push((action, res));
    }
    out
}

/// Observe the full CRI state (images + sandboxes + containers).
pub async fn snapshot(cri: &dyn CriBackend) -> Result<Snapshot, String> {
    Ok(Snapshot {
        images: cri.list_image_tags().await?,
        sandboxes: cri.list_sandboxes().await?,
        containers: cri.list_containers().await?,
    })
}

async fn run_one(cri: &dyn CriBackend, action: &Action) -> Result<(), String> {
    match action {
        Action::EnsureImage(image) => cri.pull_image(image).await,
        Action::CreateSandbox { cfg, .. } => {
            if !cfg.log_directory.is_empty() {
                tokio::fs::create_dir_all(&cfg.log_directory)
                    .await
                    .map_err(|e| format!("create log dir {}: {e}", cfg.log_directory))?;
            }
            cri.run_pod_sandbox(cfg).await.map(|_| ())
        }
        Action::CreateStartContainer {
            sandbox_id,
            ccfg,
            pcfg,
            ..
        } => {
            let id = cri.create_container(sandbox_id, ccfg, pcfg).await?;
            cri.start_container(&id).await
        }
        Action::StartContainer { id, .. } => cri.start_container(id).await,
        Action::RemoveContainer { id, .. } => {
            tolerate(cri.stop_container(id).await)?;
            tolerate(cri.remove_container(id).await)
        }
        Action::StopRemoveSandbox { id, .. } => {
            tolerate(cri.stop_pod_sandbox(id).await)?;
            tolerate(cri.remove_pod_sandbox(id).await)
        }
    }
}

/// Treat "already gone" CRI errors as success (idempotent teardown).
fn tolerate(res: Result<(), String>) -> Result<(), String> {
    match res {
        Ok(()) => Ok(()),
        Err(e) if already_gone(&e) => Ok(()),
        Err(e) => Err(e),
    }
}

fn already_gone(err: &str) -> bool {
    let low = err.to_ascii_lowercase();
    ["not found", "no such", "not exist", "already removed"]
        .iter()
        .any(|m| low.contains(m))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cri_backend::{ContainerView, SandboxView};
    use crate::sync::teardown;

    #[test]
    fn already_gone_substrings() {
        assert!(already_gone("container abc is not found"));
        assert!(already_gone("Error: No such object"));
        assert!(already_gone("sandbox does not exist"));
        assert!(!already_gone("connection refused"));
    }

    #[test]
    fn teardown_orders_containers_before_sandbox() {
        let sb = SandboxView {
            id: "sb1".into(),
            state: "SANDBOX_READY".into(),
            labels: Default::default(),
            name: "p".into(),
            namespace: "ns".into(),
            uid: "u".into(),
        };
        let c1 = ContainerView {
            id: "c2".into(),
            sandbox_id: "sb1".into(),
            state: "CONTAINER_RUNNING".into(),
            name: "x".into(),
            attempt: 0,
            labels: Default::default(),
        };
        let c2 = ContainerView {
            id: "c1".into(),
            ..c1.clone()
        };
        let mut actions = Vec::new();
        teardown(&mut actions, "ns/p", &sb, &[&c1, &c2]);
        assert_eq!(
            actions,
            vec![
                Action::RemoveContainer {
                    pod_key: "ns/p".into(),
                    id: "c1".into()
                },
                Action::RemoveContainer {
                    pod_key: "ns/p".into(),
                    id: "c2".into()
                },
                Action::StopRemoveSandbox {
                    pod_key: "ns/p".into(),
                    id: "sb1".into()
                },
            ]
        );
    }
}
