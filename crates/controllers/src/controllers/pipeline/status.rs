//! Status-write plumbing shared by the ServicePipeline + Build reconcilers
//! (TODO **T8.3**, decision **Q56**).
//!
//! No `/status` subresource exists in v1 (Q10/Q56): every status write is a
//! full-object CAS. [`write_status`] clones the object, swaps `status`,
//! skips the write when [`crate::object::semantic_eq`] says nothing changed
//! (anti-churn: a converged pipeline never rewrites itself) and returns the
//! resulting object so the caller keeps reconciling on the fresh revision.
//! A lost CAS surfaces as a conflict error and the worker requeues
//! rate-limited -- never silently dropped.
//!
//! [`bounded_log`] keeps docker output out of the status unless it is
//! bounded: only the last [`LOG_TAIL_CHARS`] characters survive.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use storage::Key;

use crate::client::Client;
use crate::controllers::pipeline::publish;
use crate::error::ControllerError;
use crate::object::{resource_version, semantic_eq};
use crate::time::now_rfc3339;

/// Status log ceiling, in characters.
pub(crate) const LOG_TAIL_CHARS: usize = 4096;

/// `obj.status` as a JSON object (empty object when absent or malformed).
pub(crate) fn status_of(obj: &Value) -> Value {
    match obj.get("status") {
        Some(status @ Value::Object(_)) => status.clone(),
        _ => json!({}),
    }
}

/// CAS-write `status` onto a clone of `obj`; a no-op when unchanged.
///
/// Returns the object the caller should keep working with: the update
/// result on a real write, the local clone otherwise.
pub(crate) async fn write_status(
    client: &Arc<dyn Client>,
    key: &Key,
    obj: &Value,
    status: Value,
) -> Result<Value, ControllerError> {
    let mut next = obj.clone();
    if !next.is_object() {
        return Err(ControllerError::Internal(format!(
            "{} is not a JSON object",
            key.as_path()
        )));
    }
    next["status"] = status;
    if semantic_eq(obj, &next) {
        return Ok(next);
    }
    client.update(key, next, resource_version(obj)).await
}

/// A condition object (`Ready`, `Completed`) stamped at `now`.
pub(crate) fn condition(ctype: &str, ok: bool, reason: &str, message: &str, now: &str) -> Value {
    json!({
        "type": ctype,
        "status": if ok { "True" } else { "False" },
        "reason": reason,
        "message": message,
        "lastUpdateTime": now,
        "lastTransitionTime": now,
    })
}

/// Current wall-clock timestamp in the repo's RFC3339 projection.
pub(crate) fn now() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    now_rfc3339(secs)
}

/// Bounded status log: the last [`LOG_TAIL_CHARS`] chars, prefixed with
/// [`publish::TRUNCATED_MARKER`] when content was dropped.
pub(crate) fn bounded_log(text: &str) -> String {
    let trimmed = text.trim_end();
    if trimmed.chars().count() <= LOG_TAIL_CHARS {
        return trimmed.to_string();
    }
    format!(
        "{}{}",
        publish::TRUNCATED_MARKER,
        publish::tail_chars(trimmed, LOG_TAIL_CHARS)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::StorageClient;
    use serde_json::json;
    use storage::EmbeddedStorage;

    #[test]
    fn bounded_log_passes_short_output_through_and_truncates_long_output() {
        assert_eq!(bounded_log("hello\n\n"), "hello");
        assert_eq!(bounded_log(""), "");
        let long = "x".repeat(LOG_TAIL_CHARS + 10);
        let tail = bounded_log(&long);
        assert!(tail.starts_with(publish::TRUNCATED_MARKER));
        assert_eq!(
            tail.chars().count(),
            publish::TRUNCATED_MARKER.chars().count() + LOG_TAIL_CHARS
        );
    }

    #[test]
    fn bounded_log_never_splits_a_code_point() {
        let long = "\u{4f60}".repeat(LOG_TAIL_CHARS + 3); // 3-byte chars
        let tail = bounded_log(&long);
        assert!(tail.starts_with(publish::TRUNCATED_MARKER));
        assert!(tail[publish::TRUNCATED_MARKER.len()..]
            .chars()
            .all(|c| c == '\u{4f60}'));
    }

    #[test]
    fn condition_shape_carries_status_and_reason() {
        let c = condition(
            "Ready",
            false,
            "BuildFailed",
            "boom",
            "2026-01-01T00:00:00Z",
        );
        assert_eq!(c["type"], json!("Ready"));
        assert_eq!(c["status"], json!("False"));
        assert_eq!(c["reason"], json!("BuildFailed"));
        assert_eq!(c["message"], json!("boom"));
        assert_eq!(c["lastTransitionTime"], json!("2026-01-01T00:00:00Z"));
    }

    #[tokio::test]
    async fn write_status_skips_unchanged_and_cas_writes_changes() {
        let store = Arc::new(EmbeddedStorage::new());
        let client: Arc<dyn Client> = Arc::new(StorageClient::new(store));
        let key = Key::new("init-pro.io", "builds", "default", "b1");
        let created = client
            .create(
                &key,
                json!({
                    "apiVersion": "init-pro.io/v1",
                    "kind": "Build",
                    "metadata": {"name": "b1", "namespace": "default"},
                    "spec": {},
                    "status": {"phase": "Pending"},
                }),
            )
            .await
            .unwrap();
        let revision = resource_version(&created);
        // Same status: no write, so the revision does not move.
        let untouched = write_status(&client, &key, &created, status_of(&created))
            .await
            .unwrap();
        assert_eq!(resource_version(&untouched), revision);
        // Changed status: CAS write against the creation revision.
        let mut status = status_of(&created);
        status["phase"] = json!("Building");
        let updated = write_status(&client, &key, &created, status).await.unwrap();
        assert_eq!(updated["status"]["phase"], json!("Building"));
        assert!(resource_version(&updated) > revision);
        // A stale CAS base conflicts (the worker requeues on it).
        let stale = write_status(&client, &key, &created, json!({"phase": "Failed"})).await;
        let err = stale.expect_err("stale write must conflict");
        assert!(err.is_conflict(), "unexpected error: {err}");
    }
}
