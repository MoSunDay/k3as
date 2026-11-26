//! Change-gated Node status heartbeats (TODO **T4.2**, long-run bug sweep
//! **B2**).
//!
//! The node Lease (`node::heartbeat`) is the liveness signal; the Node
//! object's `.status` only has to change when its CONTENT changes. Before
//! B2 every heartbeat re-PUT the whole status because `lastHeartbeatTime`
//! moved -- one durable write per node per period, forever. The pure
//! helpers here decide whether a fresh status is worth a write and keep
//! `lastTransitionTime` honest (k8s semantics: the stamp marks the last
//! flip, not the last write).

use std::time::{Duration, Instant};

use serde_json::Value;

/// Minimum spacing between two no-change status writes: the Lease carries
/// liveness, so the object only needs a periodic refresh for humans.
pub(crate) const STATUS_REFRESH_FLOOR: Duration = Duration::from_secs(300);

/// Clock stamps that move on every heartbeat without a semantic change.
const CLOCK_STAMPS: [&str; 2] = ["lastHeartbeatTime", "lastTransitionTime"];

/// The heartbeat's write gate. Immutable: [`StatusGate::due`] answers from
/// the state, [`StatusGate::mark`] returns the state after a successful
/// write (a rejected write leaves the gate untouched so the next tick
/// retries).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct StatusGate {
    last_written: Option<Instant>,
}

impl StatusGate {
    /// True when `changed`, or when the last write is older than
    /// [`STATUS_REFRESH_FLOOR`] (or there was none).
    pub(crate) fn due(&self, changed: bool, now: Instant) -> bool {
        changed
            || self
                .last_written
                .is_none_or(|t| now.duration_since(t) >= STATUS_REFRESH_FLOOR)
    }

    /// The gate after a successful write at `now`.
    pub(crate) fn mark(&self, now: Instant) -> Self {
        Self {
            last_written: Some(now),
        }
    }
}

/// True when `current` and `fresh` describe the same node status once the
/// clock stamps are scrubbed. `None` (no status yet) is never equal.
pub(crate) fn semantically_equal(current: Option<&Value>, fresh: &Value) -> bool {
    let Some(current) = current else {
        return false;
    };
    scrubbed(current) == scrubbed(fresh)
}

fn scrubbed(status: &Value) -> Value {
    let mut out = status.clone();
    if let Some(conds) = out.get_mut("conditions").and_then(Value::as_array_mut) {
        for cond in conds {
            if let Some(cond) = cond.as_object_mut() {
                for stamp in CLOCK_STAMPS {
                    cond.remove(stamp);
                }
            }
        }
    }
    out
}

/// Carry `lastTransitionTime` over from `current` into `fresh` for every
/// condition whose `status` did not flip; a new or flipped condition keeps
/// the fresh stamp. Mutates `fresh` in place (the caller owns the value).
pub(crate) fn carry_transition_times(current: Option<&Value>, fresh: &mut Value) {
    let Some(current) = current
        .and_then(|s| s.get("conditions"))
        .and_then(Value::as_array)
    else {
        return;
    };
    let Some(conds) = fresh.get_mut("conditions").and_then(Value::as_array_mut) else {
        return;
    };
    for cond in conds {
        let Some(cond) = cond.as_object_mut() else {
            continue;
        };
        let Some(prev) = current.iter().find(|p| p.get("type") == cond.get("type")) else {
            continue;
        };
        if prev.get("status") != cond.get("status") {
            continue;
        }
        if let Some(stamp) = prev.get("lastTransitionTime") {
            cond.insert("lastTransitionTime".into(), stamp.clone());
        }
    }
}

#[cfg(test)]
mod tests;
