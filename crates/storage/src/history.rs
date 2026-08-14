//! Bounded revision event log backing watch historical replay (T2.2, Q17).
//!
//! Every successful write appends exactly one `(revision, WatchEvent)` pair
//! (writes bump the revision exactly once and emit exactly one event).
//! [`EventLog::since`] serves the retained suffix so a watch can replay
//! history from `start_revision` before continuing with the live stream.
//! Retention is bounded ring-style by `capacity`, and advanced explicitly by
//! [`EventLog::compact`]; a start revision at or below the watermark surfaces
//! as [`StorageError::Compacted`] -- etcd `ErrCompacted`, upstream watch
//! `410 Gone`.

use std::collections::VecDeque;

use crate::entry::{Revision, WatchEvent};
use crate::error::StorageError;

/// Retained event history. `watermark` is the highest revision no longer
/// retained (0 = nothing dropped yet).
pub(crate) struct EventLog {
    events: VecDeque<(Revision, WatchEvent)>,
    watermark: Revision,
    capacity: usize,
}

impl EventLog {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            events: VecDeque::new(),
            watermark: 0,
            capacity,
        }
    }

    /// Append the event that occurred at `rev`. Oldest events are evicted
    /// past `capacity`, advancing the watermark (revisions are monotonic).
    pub(crate) fn push(&mut self, rev: Revision, ev: WatchEvent) {
        if self.capacity == 0 {
            self.watermark = self.watermark.max(rev);
            return;
        }
        self.events.push_back((rev, ev));
        while self.events.len() > self.capacity {
            if let Some((dropped, _)) = self.events.pop_front() {
                self.watermark = self.watermark.max(dropped);
            }
        }
    }

    /// All retained events at revisions `>= start`, in revision order, or
    /// [`StorageError::Compacted`] when `start` is at or below the watermark.
    pub(crate) fn since(&self, start: Revision) -> Result<Vec<WatchEvent>, StorageError> {
        if self.watermark > 0 && start <= self.watermark {
            return Err(StorageError::Compacted {
                requested: start,
                watermark: self.watermark,
            });
        }
        Ok(self
            .events
            .iter()
            .filter(|(rev, _)| *rev >= start)
            .map(|(_, ev)| ev.clone())
            .collect())
    }

    /// Advance the watermark to `revision` (monotonic; never lowered) and
    /// drop retained events at or below it. Returns the effective watermark.
    pub(crate) fn compact(&mut self, revision: Revision) -> Revision {
        if revision > self.watermark {
            self.watermark = revision;
            let cutoff = revision;
            self.events.retain(|(rev, _)| *rev > cutoff);
        }
        self.watermark
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn put_ev(key: &str, rev: Revision) -> WatchEvent {
        WatchEvent::Put(Arc::new(crate::entry::StoredEntry {
            key: key.to_string(),
            value: serde_json::json!({ "rev": rev }),
            create_revision: rev,
            mod_revision: rev,
            version: 1,
        }))
    }

    #[test]
    fn since_returns_retained_events_from_start_in_order() {
        let mut log = EventLog::new(16);
        for rev in 1..=4 {
            log.push(rev, put_ev("k", rev));
        }
        let evs = log.since(3).unwrap();
        assert_eq!(evs.len(), 2);
        assert_eq!(evs[0].revision(), 3);
        assert_eq!(evs[1].revision(), 4);
        // start = 1 still retained: nothing compacted yet.
        assert_eq!(log.since(1).unwrap().len(), 4);
    }

    #[test]
    fn capacity_eviction_advances_watermark_and_compacts_old_starts() {
        let mut log = EventLog::new(2);
        for rev in 1..=4 {
            log.push(rev, put_ev("k", rev));
        }
        let err = log.since(2).unwrap_err();
        assert!(
            matches!(err, StorageError::Compacted { requested: 2, watermark: 2 }),
            "got {err:?}"
        );
        // The retained suffix replays cleanly.
        assert_eq!(log.since(3).unwrap().len(), 2);
    }

    #[test]
    fn compact_drops_retained_events_and_is_monotonic() {
        let mut log = EventLog::new(16);
        for rev in 1..=3 {
            log.push(rev, put_ev("k", rev));
        }
        assert_eq!(log.compact(2), 2);
        assert!(log.since(1).unwrap_err().to_string().contains("compacted"));
        assert!(log.since(2).unwrap_err().to_string().contains("compacted"));
        assert_eq!(log.since(3).unwrap().len(), 1);
        // A lower later request must not regress the watermark.
        assert_eq!(log.compact(1), 2);
    }

    #[test]
    fn zero_capacity_forgets_everything_immediately() {
        let mut log = EventLog::new(0);
        log.push(1, put_ev("k", 1));
        assert!(matches!(
            log.since(1).unwrap_err(),
            StorageError::Compacted { requested: 1, watermark: 1 }
        ));
    }
}
