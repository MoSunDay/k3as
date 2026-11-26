//! Production compaction policy for the `rdb://` datastore (T2.3/Q33).
//!
//! `log/<rev>` is both the watch-replay history AND the only copy of past
//! revisions (`obj/` holds the live object alone), so without compaction the
//! prefix grows by one record per write for the life of the datastore. The
//! MECHANISM has existed since T2.2/T2.3 -- [`super::watch::compact`] drops
//! every `log/<rev>` at or below a watermark and persists that watermark in
//! one atomic batch, driven by [`crate::StorageBackend::compact`] -- but no
//! production caller ever asked for a compaction, so `log/` grew forever
//! (B3). This module is the missing POLICY half, kept pure so the watermark
//! arithmetic is unit-testable without a RocksDB directory:
//!
//! * [`compaction_plan`] turns "current revision" + "how many revisions must
//!   stay replayable" into the watermark to compact below;
//! * [`DEFAULT_RETAIN`] is the standing retention;
//! * [`compact_retaining`] is the backend-agnostic entry point the
//!   server-side periodic task calls (this crate owns no runtime and must not
//!   spawn threads, Q33) -- it takes `&dyn StorageBackend` because the CLI
//!   hands the server `Arc<dyn StorageBackend>`, so no downcast is needed;
//! * [`super::RdbStorage::compact_retaining`] is the same pass on the
//!   concrete backend, paired with [`super::RdbStorage::revision`].
//!
//! Retention contract: at least the latest `retain` revisions always survive,
//! so a watcher reconnecting at `current - retain + 1` still replays and CAS
//! keeps working (`obj/` records are never compacted). A start revision at or
//! below the resulting watermark answers [`crate::StorageError::Compacted`]
//! (etcd `ErrCompacted`, upstream watch `410 Gone`) so the client re-LISTs
//! instead of silently missing events; that gate lives in
//! [`super::watch::replay`].

/// Revisions of history to keep when the caller does not specify: enough for
/// a watcher to replay across a `kubectl get --watch` reconnect, bounded so
/// `log/` stays proportional to recent traffic instead of growing forever.
pub const DEFAULT_RETAIN: u64 = 1000;

/// Pure compaction plan: the watermark to compact below (inclusive), or
/// `None` when the retained history is already short enough.
///
/// `retain` counts the LATEST revisions that must stay replayable, so the
/// watermark is `current_rev - retain` and every `log/<rev>` at or below it
/// may be deleted; revisions `(watermark, current_rev]` -- exactly `retain`
/// of them -- survive. Saturating on `retain >= current_rev` (nothing to
/// drop) keeps the function total and monotonic: a bigger `retain` can only
/// lower the watermark, never raise it.
pub fn compaction_plan(current_rev: u64, retain: u64) -> Option<u64> {
    // Watermark 0 would delete nothing (revisions start at 1), so it is the
    // same plan as "nothing to do".
    current_rev.checked_sub(retain).filter(|w| *w > 0)
}

/// One compaction pass over ANY backend: keep the latest `retain` revisions
/// and drop older history. Returns the effective watermark.
///
/// `None` from [`compaction_plan`] folds to `compact(0)`, which every
/// backend answers with its standing watermark and no writes -- so a timer
/// driving this needs no special case, and the pass is always safe to call
/// (B3). The caller owns the schedule; the storage crate stays inert.
pub async fn compact_retaining(
    store: &dyn crate::backend::StorageBackend,
    retain: u64,
) -> Result<u64, crate::error::StorageError> {
    let current = store.current_revision().await?;
    let target = compaction_plan(current, retain).unwrap_or(0);
    store.compact(target).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_exactly_the_latest_retain_revisions() {
        // current 10, retain 3 -> drop 1..=7, keep 8, 9, 10.
        assert_eq!(compaction_plan(10, 3), Some(7));
        assert_eq!(compaction_plan(1_500, 1_000), Some(500));
    }

    #[test]
    fn nothing_to_do_while_history_is_short() {
        assert_eq!(compaction_plan(0, DEFAULT_RETAIN), None);
        assert_eq!(compaction_plan(1, DEFAULT_RETAIN), None);
        // current == retain leaves watermark 0, i.e. no record to drop.
        assert_eq!(compaction_plan(DEFAULT_RETAIN, DEFAULT_RETAIN), None);
    }

    #[test]
    fn retain_zero_compacts_everything_already_written() {
        // Retaining nothing is a valid (if aggressive) operator choice: the
        // watermark is the current revision, so live watches still work and
        // only replay is gone.
        assert_eq!(compaction_plan(10, 0), Some(10));
    }

    #[test]
    fn a_bigger_retain_never_raises_the_watermark() {
        // Monotonic in `retain`: asking to keep more can only compact less.
        let mut prev = u64::MAX;
        for retain in 0..=12u64 {
            if let Some(w) = compaction_plan(10, retain) {
                assert!(w <= prev, "watermark rose at retain {retain}");
                prev = w;
            }
        }
        assert_eq!(compaction_plan(10, 2), Some(8));
        assert_eq!(compaction_plan(10, 9), Some(1));
    }

    #[tokio::test]
    async fn the_dyn_pass_keeps_the_latest_revisions_on_the_embedded_store() {
        use crate::backend::StorageBackend;
        use crate::embedded::EmbeddedStorage;
        use crate::key::{Key, KeyPrefix};

        let store = EmbeddedStorage::new();
        let prefix = KeyPrefix::new("", "pods", Some("default".into()));
        for name in ["a", "b", "c", "d"] {
            store
                .create(
                    &Key::new("", "pods", "default", name),
                    serde_json::json!({ "name": name }),
                )
                .await
                .unwrap();
        }
        // Keep 2 of 4: the watermark is 2, so a watch from 1 is compacted and
        // one from 3 replays the retained window.
        assert_eq!(compact_retaining(&store, 2).await.unwrap(), 2);
        assert!(matches!(
            store.watch(&prefix, Some(1)).await,
            Err(crate::error::StorageError::Compacted { .. })
        ));
        assert!(store.watch(&prefix, Some(3)).await.is_ok());
        // A retain wider than the history reports the standing watermark
        // instead of moving it.
        assert_eq!(compact_retaining(&store, 100).await.unwrap(), 2);
    }

    #[test]
    fn a_positive_retain_always_leaves_a_replayable_window() {
        // For every non-empty plan with retain >= 1 the watermark is strictly
        // below the current revision, so at least one revision stays
        // replayable. `retain = 0` is the deliberate exception: it asks for
        // "no replay history at all" (see the case above).
        for current in 1..=64u64 {
            for retain in 1..=64u64 {
                if let Some(w) = compaction_plan(current, retain) {
                    assert!(w < current, "current {current} retain {retain} -> {w}");
                }
            }
        }
    }
}
