//! Re-LIST backoff of the kubelet's watch tasks: the pod watch of
//! [`crate::runner`] (TODO **T4.2**) and the Agent watch of
//! [`crate::agents`] (TODO **T7.1**).
//!
//! D (K10): a watch cycle used to re-LIST after a flat 500 ms whatever the
//! outcome, so an apiserver that is down (connection refused on the LIST)
//! cost two requests and two log lines per second for the whole outage.
//! [`watch_pause`] rides the shared exponential curve
//! (`crate::agent_sandbox_plan::retry_delay`: 500 ms doubling to 5 min) and
//! resets it whenever a cycle ran long enough to prove the server healthy
//! -- a long-lived watch the apiserver closes on its own schedule still
//! re-LISTs within 500 ms. Pure; the one side effect (the log line) is
//! [`log_watch_pause`], `warn` only once the pause has saturated at the cap.

use std::time::Duration;

use crate::agent_sandbox_plan::retry_delay;

/// A cycle that ran this long proves the apiserver reachable: the counter
/// starts over after it (the server closes idle watches on its own
/// schedule, which is not a failure).
pub(crate) const WATCH_CYCLE_HEALTHY: Duration = Duration::from_secs(60);

/// The counter to carry after one completed cycle, and the pause before the
/// next re-LIST (see the module docs). `failures` is the counter carried so
/// far; the first failure of a streak pauses 500 ms.
pub(crate) fn watch_pause(failures: u32, cycle_len: Duration) -> (u32, Duration) {
    let streak = if cycle_len >= WATCH_CYCLE_HEALTHY {
        0
    } else {
        failures
    };
    let streak = streak.saturating_add(1);
    (streak, retry_delay(streak))
}

/// One line per ended cycle: `debug` while the pause grows, `warn` once it
/// sits at the curve's cap (the outage is real and lasting).
pub(crate) fn log_watch_pause(task: &str, error: &str, failures: u32, pause: Duration) {
    if pause >= retry_delay(u32::MAX) {
        tracing::warn!(
            target: "init-pro",
            task,
            error,
            failures,
            pause = ?pause,
            "watch keeps ending; re-LIST paused at the backoff cap"
        );
    } else {
        tracing::debug!(
            target: "init-pro",
            task,
            error,
            failures,
            pause = ?pause,
            "watch cycle ended; re-LIST after the backoff pause"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// D: consecutive short cycles back off along the shared curve; a long
    /// cycle resets the streak, so a healthy watch still re-LISTs at 500 ms.
    #[test]
    fn watch_pause_backs_off_short_cycles_and_resets_on_long_ones() {
        let short = Duration::from_secs(1);
        assert_eq!(
            watch_pause(0, short),
            (1, Duration::from_millis(500)),
            "the first failure keeps today's 500 ms"
        );
        let (f, p) = watch_pause(1, short);
        assert_eq!((f, p), (2, Duration::from_secs(1)));
        assert_eq!(watch_pause(2, short), (3, Duration::from_secs(2)));
        let long = WATCH_CYCLE_HEALTHY + Duration::from_secs(1);
        assert_eq!(
            watch_pause(9, long),
            (1, Duration::from_millis(500)),
            "a long cycle proves the server healthy"
        );
        assert_eq!(
            watch_pause(9, WATCH_CYCLE_HEALTHY),
            (1, Duration::from_millis(500)),
            "the threshold is inclusive"
        );
        // Saturation: the pause never passes the 5 min cap, the counter
        // never overflows.
        let (mut f, mut p) = (0u32, Duration::ZERO);
        for _ in 0..40 {
            let next = watch_pause(f, short);
            f = next.0;
            p = next.1;
        }
        assert_eq!((f, p), (40, Duration::from_secs(300)));
        assert_eq!(
            watch_pause(u32::MAX, short),
            (u32::MAX, Duration::from_secs(300))
        );
    }
}
