//! Bounded shutdown drain for the kubelet task set (TODO **T4.2**, C2).
//!
//! [`crate::runner::spawn`] hands back the loops' join handles and the agent
//! runtime awaits them while shutting down (`crates/cli/src/runtime.rs`).
//! That join had NO upper bound, so a single wedged task could hold the whole
//! agent's drain open forever -- the read side is now bounded
//! (`http::BODY_READ_TIMEOUT` per chunk, `PULL_DEADLINE` per pull, and the
//! sync loop races its cycle against the shutdown token), but any other
//! blocking await is a candidate. [`drain`] bounds it structurally: every
//! handle gets [`DRAIN_JOIN_TIMEOUT`] to finish, then the task is ABORTED
//! with a warning that names it, so the agent always reaches its runtime
//! shutdown (containerd/flannel teardown) instead of hanging on a zombie.

use std::time::Duration;

use tokio::task::JoinHandle;

/// C2: upper bound on ONE task's shutdown join.
pub const DRAIN_JOIN_TIMEOUT: Duration = Duration::from_secs(10);

/// [`crate::runner::spawn`]'s tasks in spawn order, so a warning names the
/// wedged loop instead of an index.
pub const TASK_NAMES: [&str; 5] = ["pod-watch", "sync", "node", "log-exec-api", "log-rotate"];

/// How one bounded join ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DrainOutcome {
    /// The task returned on its own.
    Finished,
    /// The task panicked (or was already cancelled) before the join.
    Failed(String),
    /// The budget expired; the task was aborted and abandoned.
    Abandoned,
}

/// Join ONE task under an explicit budget (the pure seam the unit tests
/// drive): `Ok(())` -> [`DrainOutcome::Finished`], a panic/cancel error ->
/// [`DrainOutcome::Failed`], an expired budget -> abort + [`DrainOutcome::Abandoned`].
pub async fn join_bounded_within(
    handle: JoinHandle<()>,
    name: &str,
    budget: Duration,
) -> DrainOutcome {
    // Polled by reference so an expired budget still owns the handle and can
    // ABORT the wedged loop instead of detaching it.
    let mut handle = handle;
    match tokio::time::timeout(budget, &mut handle).await {
        Ok(Ok(())) => DrainOutcome::Finished,
        Ok(Err(e)) => DrainOutcome::Failed(e.to_string()),
        Err(_) => {
            tracing::warn!(
                target: "init-pro",
                task = name,
                budget = ?budget,
                "kubelet: task did not stop within the drain budget; abandoning it"
            );
            handle.abort();
            let _ = handle.await; // reap the cancelled task
            DrainOutcome::Abandoned
        }
    }
}

/// [`join_bounded_within`] with the default [`DRAIN_JOIN_TIMEOUT`].
pub async fn join_bounded(handle: JoinHandle<()>, name: &str) -> DrainOutcome {
    join_bounded_within(handle, name, DRAIN_JOIN_TIMEOUT).await
}

/// Drain a whole spawn set, in order, each under [`DRAIN_JOIN_TIMEOUT`].
/// Returns how many tasks were abandoned (0 = a clean drain). Ordering is
/// preserved: the sync task still gets to run its sandbox teardown before
/// the caller stops containerd.
pub async fn drain(handles: Vec<JoinHandle<()>>) -> usize {
    let mut abandoned = 0usize;
    for (i, handle) in handles.into_iter().enumerate() {
        let name = TASK_NAMES.get(i).copied().unwrap_or("kubelet-task");
        if join_bounded(handle, name).await == DrainOutcome::Abandoned {
            abandoned += 1;
        }
    }
    if abandoned > 0 {
        tracing::warn!(
            target: "init-pro",
            abandoned,
            "kubelet: drained with abandoned tasks (shutdown continues)"
        );
    }
    abandoned
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pending_task() -> JoinHandle<()> {
        tokio::spawn(async {
            std::future::pending::<()>().await;
        })
    }

    #[tokio::test]
    async fn a_finished_task_is_not_abandoned() {
        let h = tokio::spawn(async {});
        assert_eq!(
            join_bounded(h, "done").await,
            DrainOutcome::Finished,
            "the default 10s budget never trips a completed task"
        );
    }

    #[tokio::test]
    async fn a_stalled_task_is_abandoned_within_the_budget() {
        let started = std::time::Instant::now();
        let outcome =
            join_bounded_within(pending_task(), "stalled", Duration::from_millis(50)).await;
        assert_eq!(outcome, DrainOutcome::Abandoned);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the bound is the budget, not the caller's patience"
        );
    }

    #[tokio::test]
    async fn a_panicking_task_reports_failed() {
        let h = tokio::spawn(async { panic!("simulated loop panic") });
        match join_bounded(h, "boom").await {
            DrainOutcome::Failed(e) => assert!(!e.is_empty()),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn drain_counts_only_the_abandoned_tasks() {
        // A clean set drains with 0 abandoned; the ordering-sensitive face
        // walks every handle.
        assert_eq!(
            drain(vec![tokio::spawn(async {}), tokio::spawn(async {})]).await,
            0
        );
        // Mixed set through the per-handle seam (so the stalled one gets a
        // 50ms budget instead of the 10s default): exactly one abandoned.
        let outcomes = [
            join_bounded_within(tokio::spawn(async {}), "done", DRAIN_JOIN_TIMEOUT).await,
            join_bounded_within(pending_task(), TASK_NAMES[1], Duration::from_millis(50)).await,
            join_bounded_within(tokio::spawn(async {}), "done", DRAIN_JOIN_TIMEOUT).await,
        ];
        assert_eq!(
            outcomes
                .iter()
                .filter(|o| **o == DrainOutcome::Abandoned)
                .count(),
            1,
            "exactly the stalled task is abandoned: {outcomes:?}"
        );
    }

    #[test]
    fn drain_budget_and_names_are_declared() {
        assert_eq!(DRAIN_JOIN_TIMEOUT, Duration::from_secs(10));
        assert_eq!(TASK_NAMES.len(), 5, "one name per spawned loop");
    }
}
