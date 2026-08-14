//! Deployment status.condition builders + anti-churn merge (TODO **T3.1b**).
//!
//! Conditions are the dependency surface for `kubectl rollout status`:
//! `Available` (MinimumReplicasAvailable/Unavailable) and `Progressing`
//! (ReplicaSetUpdated / NewReplicaSetAvailable / ProgressDeadlineExceeded).
//! JSON-only wire (decision **Q10**). [`merge_conditions`] implements the
//! upstream timestamp semantics -- frozen when nothing changed, new
//! `lastUpdateTime` on content change, new `lastTransitionTime` only on a
//! status flip -- so a converged Deployment never rewrites its status (the
//! anti-oscillation guarantee the quiesce test pins).

use serde_json::{json, Value};

use crate::time::parse_rfc3339;

/// A `Progressing` condition: `status` True/False from the bool.
pub(crate) fn progressing(now: &str, status: bool, reason: &str, message: &str) -> Value {
    condition("Progressing", now, status, reason, message)
}

/// An `Available` condition: `status` True/False from the bool.
pub(crate) fn available(now: &str, status: bool, reason: &str, message: &str) -> Value {
    condition("Available", now, status, reason, message)
}

fn condition(ctype: &str, now: &str, status: bool, reason: &str, message: &str) -> Value {
    json!({
        "type": ctype,
        "status": if status { "True" } else { "False" },
        "reason": reason,
        "message": message,
        "lastUpdateTime": now,
        "lastTransitionTime": now,
    })
}

/// First condition of `ctype`, if any.
pub(crate) fn find_condition<'a>(conditions: &'a [Value], ctype: &str) -> Option<&'a Value> {
    conditions
        .iter()
        .find(|c| c.get("type").and_then(Value::as_str) == Some(ctype))
}

/// Anti-churn merge of `desired` onto `existing` (upstream semantics):
///
/// * same type with equal status, reason AND message -> existing verbatim
///   (timestamps frozen: quiesced Deployments never rewrite);
/// * content changed but status equal -> desired content, `lastUpdateTime`
///   = `now`, old `lastTransitionTime` kept (progress events -- e.g. a new
///   target ReplicaSet changing the message -- refresh the deadline clock);
/// * status differs -> desired content with BOTH timestamps = `now`;
/// * new type -> desired content with BOTH timestamps = `now`.
///
/// Output order follows `desired`; existing conditions of other types are
/// dropped (the Deployment controller always emits the full set).
pub(crate) fn merge_conditions(existing: &[Value], desired: &[Value], now: &str) -> Vec<Value> {
    desired
        .iter()
        .map(|d| {
            let dtype = d.get("type").and_then(Value::as_str).unwrap_or("");
            let Some(old) = find_condition(existing, dtype) else {
                return with_timestamps(d, now, now);
            };
            let status_eq = old.get("status") == d.get("status");
            let content_eq = status_eq
                && old.get("reason") == d.get("reason")
                && old.get("message") == d.get("message");
            if content_eq {
                old.clone() // frozen verbatim
            } else if status_eq {
                // Content-only change: fresh update time, transition kept.
                let transition = old
                    .get("lastTransitionTime")
                    .and_then(Value::as_str)
                    .unwrap_or(now);
                with_timestamps(d, now, transition)
            } else {
                with_timestamps(d, now, now)
            }
        })
        .collect()
}

/// `d` with fresh timestamps (string Values in, string Values out).
fn with_timestamps(d: &Value, update: &str, transition: &str) -> Value {
    let mut out = d.clone();
    out["lastUpdateTime"] = json!(update);
    out["lastTransitionTime"] = json!(transition);
    out
}

/// True when the `Progressing` condition exists and `now_secs` is past its
/// `lastUpdateTime` + `deadline_secs` (absent/unparseable -> false).
pub(crate) fn deadline_exceeded(conditions: &[Value], now_secs: u64, deadline_secs: u64) -> bool {
    let Some(p) = find_condition(conditions, "Progressing") else {
        return false;
    };
    let Some(updated) = p
        .get("lastUpdateTime")
        .and_then(Value::as_str)
        .and_then(parse_rfc3339)
    else {
        return false;
    };
    now_secs > updated.saturating_add(deadline_secs)
}

/// True when the deployment already carries `Progressing=False /
/// ProgressDeadlineExceeded`. Once a rollout has failed its deadline it
/// STAYS failed until it completes (or is un-stuck) -- re-evaluating the
/// deadline from the fresh write time would flap it back to
/// ReplicaSetUpdated every resync tick.
pub(crate) fn progress_deadline_exceeded(conditions: &[Value]) -> bool {
    find_condition(conditions, "Progressing").is_some_and(|p| {
        p.get("status").and_then(Value::as_str) == Some("False")
            && p.get("reason").and_then(Value::as_str) == Some("ProgressDeadlineExceeded")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: &str = "2026-01-01T00:00:00Z";
    const T1: &str = "2026-01-02T00:00:00Z";

    #[test]
    fn builders_emit_upstream_shape() {
        let p = progressing(T0, true, "ReplicaSetUpdated", "m");
        assert_eq!(p["type"], "Progressing");
        assert_eq!(p["status"], "True");
        assert_eq!(p["reason"], "ReplicaSetUpdated");
        assert_eq!(p["lastUpdateTime"], T0);
        assert_eq!(p["lastTransitionTime"], T0);
        let a = available(T0, false, "MinimumReplicasUnavailable", "m");
        assert_eq!(a["type"], "Available");
        assert_eq!(a["status"], "False");
    }

    #[test]
    fn merge_freezes_unchanged_conditions_verbatim() {
        // THE anti-oscillation guarantee: identical content keeps the old
        // object byte-for-byte, timestamps included.
        let existing = vec![progressing(T0, true, "ReplicaSetUpdated", "msg")];
        let desired = vec![progressing(T1, true, "ReplicaSetUpdated", "msg")];
        let merged = merge_conditions(&existing, &desired, T1);
        assert_eq!(merged[0], existing[0]);
    }

    #[test]
    fn merge_message_change_refreshes_update_time_only() {
        // A progress event (message names a NEW target ReplicaSet) must
        // restart the progress-deadline clock without a status flip.
        let existing = vec![progressing(T0, true, "ReplicaSetUpdated", "old msg")];
        let desired = vec![progressing(T1, true, "ReplicaSetUpdated", "new msg")];
        let merged = merge_conditions(&existing, &desired, T1);
        assert_eq!(merged[0]["message"], "new msg");
        assert_eq!(merged[0]["lastUpdateTime"], T1);
        assert_eq!(merged[0]["lastTransitionTime"], T0);
    }

    #[test]
    fn merge_status_flip_resets_both_timestamps() {
        let existing = vec![progressing(T0, true, "ReplicaSetUpdated", "m")];
        let desired = vec![progressing(T1, false, "ProgressDeadlineExceeded", "m2")];
        let merged = merge_conditions(&existing, &desired, T1);
        assert_eq!(merged[0]["status"], "False");
        assert_eq!(merged[0]["reason"], "ProgressDeadlineExceeded");
        assert_eq!(merged[0]["lastUpdateTime"], T1);
        assert_eq!(merged[0]["lastTransitionTime"], T1);
    }

    #[test]
    fn merge_reason_change_keeps_transition_time() {
        let existing = vec![progressing(T0, true, "ReplicaSetUpdated", "m")];
        let desired = vec![progressing(T1, true, "NewReplicaSetAvailable", "m2")];
        let merged = merge_conditions(&existing, &desired, T1);
        assert_eq!(merged[0]["lastUpdateTime"], T1);
        assert_eq!(merged[0]["lastTransitionTime"], T0);
    }

    #[test]
    fn merge_new_type_gets_now_and_order_follows_desired() {
        let existing = vec![progressing(T0, true, "ReplicaSetUpdated", "m")];
        let desired = vec![
            available(T1, true, "MinimumReplicasAvailable", "a"),
            progressing(T1, true, "ReplicaSetUpdated", "m"),
        ];
        let merged = merge_conditions(&existing, &desired, T1);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0]["type"], "Available");
        assert_eq!(merged[0]["lastUpdateTime"], T1);
        assert_eq!(merged[0]["lastTransitionTime"], T1);
        assert_eq!(merged[1]["type"], "Progressing");
    }

    #[test]
    fn deadline_needs_parseable_progressing_last_update() {
        let t = 1_700_000_000u64;
        let ts = crate::time::now_rfc3339(t);
        let conds = vec![progressing(&ts, true, "ReplicaSetUpdated", "m")];
        assert!(!deadline_exceeded(&conds, t + 10, 600));
        assert!(deadline_exceeded(&conds, t + 601, 600));
        // Absent Progressing / unparseable timestamp -> false.
        assert!(!deadline_exceeded(&[], t + 10_000, 600));
        let garbage = vec![json!({"type": "Progressing", "status": "True",
            "lastUpdateTime": "not-a-time"})];
        assert!(!deadline_exceeded(&garbage, t + 10_000, 600));
    }

    #[test]
    fn find_condition_matches_type() {
        let conds = vec![
            available(T0, true, "r", "m"),
            progressing(T0, true, "r", "m"),
        ];
        assert_eq!(
            find_condition(&conds, "Progressing").and_then(|c| c.get("type")),
            Some(&json!("Progressing"))
        );
        assert!(find_condition(&conds, "Bogus").is_none());
    }

    #[test]
    fn progress_deadline_exceeded_is_sticky_shape_only() {
        let exceeded = vec![progressing(T0, false, "ProgressDeadlineExceeded", "m")];
        assert!(progress_deadline_exceeded(&exceeded));
        // Merely past the deadline is not enough -- the condition must
        // already say it.
        let progressing_only = vec![progressing(T0, true, "ReplicaSetUpdated", "m")];
        assert!(!progress_deadline_exceeded(&progressing_only));
        assert!(!progress_deadline_exceeded(&[]));
    }
}
