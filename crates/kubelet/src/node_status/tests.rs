//! B2 heartbeat gate cases: clock-only differences never count as change,
//! transition stamps survive unchanged conditions, and the floor keeps a
//! quiet node's status fresh at a low rate.

use std::time::Duration;

use serde_json::json;

use super::*;

fn status(heartbeat: &str, transition: &str, ready: &str) -> Value {
    json!({
        "capacity": {"pods": "110"},
        "conditions": [{"type": "Ready", "status": ready,
                        "lastHeartbeatTime": heartbeat, "lastTransitionTime": transition}],
        "nodeInfo": {"kubeletVersion": "init-pro-0.2.0"},
    })
}

#[test]
fn clock_only_difference_is_not_a_change() {
    let a = status("2026-11-26T00:00:00Z", "2026-11-26T00:00:00Z", "True");
    let b = status("2026-11-26T00:00:10Z", "2026-11-26T00:00:10Z", "True");
    assert!(semantically_equal(Some(&a), &b));
}

#[test]
fn content_changes_are_changes() {
    let a = status("t1", "t1", "True");
    assert!(!semantically_equal(Some(&a), &status("t1", "t1", "False")));
    let mut more = status("t1", "t1", "True");
    more["capacity"]["nvidia.com/gpu"] = json!("2");
    assert!(!semantically_equal(Some(&a), &more));
    // A missing status (first heartbeat) is never "equal".
    assert!(!semantically_equal(None, &a));
}

#[test]
fn transition_time_carries_over_unchanged_conditions_only() {
    let current = status("t9", "2026-11-01T00:00:00Z", "True");
    let mut fresh = status("2026-11-26T00:00:00Z", "2026-11-26T00:00:00Z", "True");
    carry_transition_times(Some(&current), &mut fresh);
    let cond = &fresh["conditions"][0];
    assert_eq!(cond["lastTransitionTime"], "2026-11-01T00:00:00Z");
    assert_eq!(cond["lastHeartbeatTime"], "2026-11-26T00:00:00Z");

    let mut flipped = status("2026-11-26T00:00:00Z", "2026-11-26T00:00:00Z", "False");
    carry_transition_times(Some(&current), &mut flipped);
    assert_eq!(
        flipped["conditions"][0]["lastTransitionTime"],
        "2026-11-26T00:00:00Z"
    );
    // No current status / no conditions: total, no panic.
    let mut orphan = status("t", "t", "True");
    carry_transition_times(None, &mut orphan);
    carry_transition_times(Some(&json!({})), &mut orphan);
    assert_eq!(orphan["conditions"][0]["lastTransitionTime"], "t");
}

#[test]
fn gate_writes_changes_immediately_and_quiet_status_at_the_floor() {
    let t0 = Instant::now();
    let gate = StatusGate::default();
    assert!(gate.due(false, t0), "the first status must be written");
    assert!(gate.due(true, t0), "a change is always due");
    // A pass that did not write leaves the gate value untouched.
    assert_eq!(gate, StatusGate::default());
    let written = gate.mark(t0);
    assert!(!written.due(false, t0 + Duration::from_secs(299)));
    assert!(written.due(false, t0 + STATUS_REFRESH_FLOOR));
    assert!(written.due(true, t0 + Duration::from_secs(1)));
}
