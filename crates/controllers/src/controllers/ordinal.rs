//! StatefulSet ordinal math (TODO **T3.1b**): the pure identity/decision
//! functions shared by the reconciler in [`super::statefulset`] -- pod/PVC
//! name derivation, `<sts>-<ordinal>` parsing, scale-down/rolling-update
//! delete decisions and the ControllerRevision bump. Pure and unit-tested
//! here so `statefulset.rs` stays the reconcile flow (the same split as
//! deployment.rs -> rollout.rs). JSON-only wire (Q10).

use serde_json::Value;

/// Label carrying a pod's ControllerRevision hash (upstream name).
pub(crate) const REVISION_HASH_LABEL: &str = "controller-revision-hash";

/// Pod name of ordinal `ordinal` under `sts_name`.
pub(crate) fn pod_name(sts_name: &str, ordinal: i64) -> String {
    format!("{sts_name}-{ordinal}")
}

/// PVC name derived from one claim template + the pod ordinal (upstream
/// `<claim>-<sts>-<ordinal>`).
pub(crate) fn pvc_name(claim: &str, sts_name: &str, ordinal: i64) -> String {
    format!("{claim}-{sts_name}-{ordinal}")
}

/// Trailing integer ordinal of a `<sts>-<ordinal>` pod name; `None` for
/// foreign names, non-numeric or empty suffixes.
pub(crate) fn ordinal_of(pod: &Value, sts_name: &str) -> Option<i64> {
    let rest = pod
        .pointer("/metadata/name")?
        .as_str()?
        .strip_prefix(sts_name)?
        .strip_prefix('-')?;
    (!rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()))
        .then(|| rest.parse().ok())
        .flatten()
}

/// `metadata.labels[controller-revision-hash]`, if present.
pub(crate) fn revision_hash_of(obj: &Value) -> Option<&str> {
    obj.pointer(&format!("/metadata/labels/{REVISION_HASH_LABEL}"))
        .and_then(Value::as_str)
}

/// Delete decision for one owned pod: scale-down surplus, or (rolling
/// update) a stale revision at an ordinal >= partition.
pub(crate) fn should_delete(
    ordinal: i64,
    desired: i64,
    rolling: bool,
    partition: i64,
    hash_label: Option<&str>,
    short: &str,
) -> bool {
    ordinal >= desired || (rolling && ordinal >= partition && hash_label != Some(short))
}

/// `max(existing revision) + 1`, starting at 1 (ControllerRevision math).
pub(crate) fn next_revision(existing: &[Value]) -> u64 {
    existing
        .iter()
        .filter_map(|r| r.pointer("/revision").and_then(Value::as_u64))
        .max()
        .unwrap_or(0)
        + 1
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn pod_named(n: &str) -> Value {
        json!({"metadata": {"name": n}})
    }

    #[test]
    fn ordinal_and_name_derivation() {
        let ord = |n: &str| ordinal_of(&pod_named(n), "web");
        assert_eq!(ord("web-0"), Some(0));
        assert_eq!(ord("web-42"), Some(42));
        assert_eq!(ordinal_of(&pod_named("web2-1"), "web2"), Some(1)); // own prefix
        for foreign in ["web-x", "web-", "web-1-x", "other-1", "web", "web2-1"] {
            assert_eq!(ord(foreign), None);
        }
        assert_eq!(pvc_name("data", "web", 2), "data-web-2");
        assert_eq!(pod_name("web", 0), "web-0");
        assert_eq!(revision_hash_of(&pod_named("web-1")), None);
    }

    #[test]
    fn partition_and_scale_down_delete_decisions() {
        let short = "aaaaaaaaaa";
        // Scale-down: ordinals >= desired always go.
        assert!(should_delete(2, 2, false, 0, Some(short), short));
        assert!(!should_delete(1, 2, false, 0, Some("other"), short));
        // RollingUpdate at partition 1: stale ordinals >= 1 roll, 0 is pinned.
        assert!(should_delete(1, 2, true, 1, Some("old"), short));
        assert!(should_delete(1, 2, true, 0, None, short));
        assert!(!should_delete(0, 2, true, 1, Some("old"), short));
        // Current revision is never deleted for update reasons; OnDelete
        // never deletes for update reasons.
        assert!(!should_delete(1, 2, true, 0, Some(short), short));
        assert!(!should_delete(1, 2, false, 0, Some("old"), short));
    }

    #[test]
    fn revision_bumps_from_owned_maximum() {
        assert_eq!(next_revision(&[]), 1);
        let revs = vec![json!({"revision": 3}), json!({"revision": 7}), json!({})];
        assert_eq!(next_revision(&revs), 8);
    }
}
