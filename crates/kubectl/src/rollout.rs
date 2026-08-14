//! `kubectl rollout status` state machine (T3.1b, Q21).
//!
//! Two layers, deliberately split:
//! - [`evaluate`] is a pure function over a Deployment JSON object and
//!   implements the kubectl rollout-status semantics (observedGeneration,
//!   progress deadline, complete, waiting messages). Never panics on
//!   missing/odd fields — everything defaults.
//! - [`rollout_status`] drives the Q21 poll loop (plain GET every 250ms
//!   instead of a watch stream), printing each NEW waiting message as it
//!   changes, and maps the final state to an [`Outcome`].

use std::io::Write;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::http::HttpClient;

/// Poll cadence for the Q21 poll-GET loop.
const POLL_INTERVAL: Duration = Duration::from_millis(250);
/// Exact kubectl timeout text.
pub(crate) const TIMEOUT_MSG: &str = "error: timed out waiting for the condition";

/// Terminal result of `rollout status`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// Deployment fully rolled out (exit 0).
    Success(String),
    /// Hard error: not found, deadline exceeded, transport failure (exit 1).
    Failure(String),
    /// `--timeout` elapsed while still waiting (exit 1).
    Timeout(String),
}

/// One evaluation step of the rollout state machine (pure).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Eval {
    /// True when the rollout reached a terminal state (complete or deadline).
    pub(crate) done: bool,
    /// kubectl-style message for the current state.
    pub(crate) message: String,
    /// True when `done` came from `ProgressDeadlineExceeded` (an error).
    pub(crate) hard_error: bool,
}

/// `v["field"].as_u64()` with a 0 default — rollout inputs are advisory.
fn field_u64(v: &Value, field: &str) -> u64 {
    v.get(field).and_then(Value::as_u64).unwrap_or(0)
}

/// Evaluate a Deployment object, kubectl `rollout status` semantics.
///
/// Order: spec-not-observed -> progress deadline -> complete -> waiting.
pub(crate) fn evaluate(dep: &Value) -> Eval {
    // serde_json indexing yields Null for missing paths — no panics possible.
    let name = dep["metadata"]["name"].as_str().unwrap_or("");
    let generation = field_u64(&dep["metadata"], "generation");
    // spec.replicas defaults to 1 when absent or not an integer.
    let spec_replicas = if dep["spec"]["replicas"].is_u64() {
        field_u64(&dep["spec"], "replicas")
    } else {
        1
    };
    let status = &dep["status"];
    let observed = field_u64(status, "observedGeneration");
    let updated = field_u64(status, "updatedReplicas");
    let replicas = field_u64(status, "replicas");
    let available = field_u64(status, "availableReplicas");

    if observed < generation {
        return Eval {
            done: false,
            message: "Waiting for deployment spec update to be observed".to_string(),
            hard_error: false,
        };
    }

    let deadline_exceeded = status["conditions"].as_array().is_some_and(|conds| {
        conds.iter().any(|c| {
            c["type"].as_str() == Some("Progressing")
                && c["reason"].as_str() == Some("ProgressDeadlineExceeded")
        })
    });
    if deadline_exceeded {
        return Eval {
            done: true,
            message: format!("deployment \"{name}\" exceeded its progress deadline"),
            hard_error: true,
        };
    }

    if updated == spec_replicas
        && replicas == spec_replicas
        && available == spec_replicas
        && observed >= generation
    {
        return Eval {
            done: true,
            message: format!("deployment \"{name}\" successfully rolled out"),
            hard_error: false,
        };
    }

    let detail = if updated < spec_replicas {
        format!("{updated} of {spec_replicas} new replicas have been updated")
    } else if replicas > updated {
        format!(
            "{} old replicas are pending termination",
            replicas - updated
        )
    } else {
        format!("{available} of {spec_replicas} updated replicas are available")
    };
    Eval {
        done: false,
        message: format!("Waiting for deployment \"{name}\" rollout to finish: {detail}"),
        hard_error: false,
    }
}

/// Poll the deployment until the rollout finishes, the deadline trips, or
/// `timeout` elapses. Waiting messages go to stdout as they change; the
/// final success line is printed to stdout, terminal errors are returned in
/// the [`Outcome`] for the caller to route to stderr.
pub(crate) async fn rollout_status(
    http: &HttpClient,
    ns: &str,
    name: &str,
    timeout: Option<Duration>,
) -> Outcome {
    let path = format!("/apis/apps/v1/namespaces/{ns}/deployments/{name}");
    let single_attempt = timeout == Some(Duration::ZERO);
    let deadline = timeout
        .filter(|t| *t > Duration::ZERO)
        .map(|t| Instant::now() + t);
    let mut last_message: Option<String> = None;

    loop {
        match http.get_json(&path).await {
            Ok((status, body)) => match status {
                200 => {
                    let eval = evaluate(&body);
                    if !eval.done && last_message.as_deref() != Some(eval.message.as_str()) {
                        println!("{}", eval.message);
                        let _ = std::io::stdout().flush();
                        last_message = Some(eval.message.clone());
                    }
                    if eval.hard_error {
                        return Outcome::Failure(format!("error: {}", eval.message));
                    }
                    if eval.done {
                        println!("{}", eval.message);
                        let _ = std::io::stdout().flush();
                        return Outcome::Success(eval.message);
                    }
                }
                404 => {
                    return Outcome::Failure(format!(
                        "Error from server (NotFound): deployments \"{name}\" not found"
                    ))
                }
                code => return Outcome::Failure(server_error(code, &body)),
            },
            Err(e) => return Outcome::Failure(connection_message(http, &e)),
        }

        if single_attempt {
            return Outcome::Timeout(TIMEOUT_MSG.to_string());
        }
        match deadline {
            Some(d) => {
                let now = Instant::now();
                if now >= d {
                    return Outcome::Timeout(TIMEOUT_MSG.to_string());
                }
                tokio::time::sleep((d - now).min(POLL_INTERVAL)).await;
            }
            None => tokio::time::sleep(POLL_INTERVAL).await,
        }
    }
}

/// kubectl-style `Error from server: <message>` for non-200/404 responses.
fn server_error(code: u16, body: &Value) -> String {
    match body["message"].as_str() {
        Some(m) => format!("Error from server: {m}"),
        None => format!("Error from server: HTTP {code}"),
    }
}

/// kubectl-style connection diagnostics (refused is the common misconfig).
fn connection_message(http: &HttpClient, err: &str) -> String {
    if err.to_ascii_lowercase().contains("refused") {
        format!(
            "The connection to the server {}:{} was refused - did you specify the right host or port?",
            http.host, http.port
        )
    } else {
        format!("error: {err}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deployment(name: &str, spec_replicas: u64, status: Value) -> Value {
        serde_json::json!({
            "apiVersion": "apps/v1",
            "kind": "Deployment",
            "metadata": {"name": name, "namespace": "default", "generation": 1},
            "spec": {"replicas": spec_replicas},
            "status": status,
        })
    }

    #[test]
    fn evaluate_complete_deployment_is_done() {
        let dep = deployment(
            "r",
            2,
            serde_json::json!({"observedGeneration": 1, "replicas": 2, "updatedReplicas": 2, "availableReplicas": 2}),
        );
        let e = evaluate(&dep);
        assert!(e.done && !e.hard_error);
        assert_eq!(e.message, "deployment \"r\" successfully rolled out");
    }

    #[test]
    fn evaluate_scale_to_zero_is_complete() {
        let dep = deployment("r", 0, serde_json::json!({"observedGeneration": 1}));
        let e = evaluate(&dep);
        assert!(e.done);
        assert_eq!(e.message, "deployment \"r\" successfully rolled out");
    }

    #[test]
    fn evaluate_waiting_new_replicas_updated() {
        let dep = deployment(
            "r",
            3,
            serde_json::json!({"observedGeneration": 1, "replicas": 1, "updatedReplicas": 1, "availableReplicas": 1}),
        );
        let e = evaluate(&dep);
        assert!(!e.done && !e.hard_error);
        assert_eq!(
            e.message,
            "Waiting for deployment \"r\" rollout to finish: 1 of 3 new replicas have been updated"
        );
    }

    #[test]
    fn evaluate_waiting_old_replicas_termination() {
        let dep = deployment(
            "r",
            3,
            serde_json::json!({"observedGeneration": 1, "replicas": 4, "updatedReplicas": 3, "availableReplicas": 3}),
        );
        let e = evaluate(&dep);
        assert!(!e.done);
        assert_eq!(
            e.message,
            "Waiting for deployment \"r\" rollout to finish: 1 old replicas are pending termination"
        );
    }

    #[test]
    fn evaluate_waiting_available_replicas() {
        let dep = deployment(
            "r",
            3,
            serde_json::json!({"observedGeneration": 1, "replicas": 3, "updatedReplicas": 3, "availableReplicas": 2}),
        );
        let e = evaluate(&dep);
        assert!(!e.done);
        assert_eq!(
            e.message,
            "Waiting for deployment \"r\" rollout to finish: 2 of 3 updated replicas are available"
        );
    }

    #[test]
    fn evaluate_progress_deadline_exceeded_is_hard_error() {
        let dep = deployment(
            "r",
            3,
            serde_json::json!({
                "observedGeneration": 1, "replicas": 3, "updatedReplicas": 3, "availableReplicas": 2,
                "conditions": [{"type": "Available", "status": "False"},
                               {"type": "Progressing", "reason": "ProgressDeadlineExceeded"}]
            }),
        );
        let e = evaluate(&dep);
        assert!(e.done && e.hard_error);
        assert_eq!(e.message, "deployment \"r\" exceeded its progress deadline");
    }

    #[test]
    fn evaluate_spec_not_observed_wins_over_everything() {
        let dep = deployment(
            "r",
            2,
            serde_json::json!({
                "observedGeneration": 0, "replicas": 2, "updatedReplicas": 2, "availableReplicas": 2,
                "conditions": [{"type": "Progressing", "reason": "ProgressDeadlineExceeded"}]
            }),
        );
        let e = evaluate(&dep);
        assert!(!e.done && !e.hard_error);
        assert_eq!(
            e.message,
            "Waiting for deployment spec update to be observed"
        );
    }

    #[test]
    fn evaluate_defaults_never_panic() {
        // Bare minimum object: defaults must apply (spec.replicas -> 1,
        // status counts -> 0) without panicking.
        let dep = serde_json::json!({"metadata": {"name": "r"}});
        let e = evaluate(&dep);
        assert!(!e.done && !e.hard_error);
        assert_eq!(
            e.message,
            "Waiting for deployment \"r\" rollout to finish: 0 of 1 new replicas have been updated"
        );

        // Odd types (strings/floats where u64 belongs) also fall back safely.
        let weird = serde_json::json!({
            "metadata": {"name": "w", "generation": 2},
            "spec": {"replicas": "three"},
            "status": {"observedGeneration": null, "replicas": 1.5, "updatedReplicas": 1}
        });
        let e = evaluate(&weird);
        assert!(!e.done);
        assert_eq!(
            e.message,
            "Waiting for deployment spec update to be observed"
        );
    }
}
