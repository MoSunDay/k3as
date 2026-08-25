//! Argo Workflows `CronWorkflow` types (TODO **T7.3 Scope A**, decision **Q30**).
//!
//! 1:1 typed port of upstream `cron_workflow_types.go` v4 (`CronWorkflow`,
//! `CronWorkflowList`, `CronWorkflowSpec`, `StopStrategy`,
//! `CronWorkflowStatus`). Field SSOT is the Go json tags; wire format is
//! JSON-only (decision **Q10**). Data-only port: the Go schedule helpers
//! (`GetScheduleString`, `HasActiveUID`, ...) are not carried.
//! `ConcurrencyPolicy`/`CronWorkflowPhase` live in [`super::phases`].

use k8s_openapi::api::core::v1 as apiv1;
use k8s_openapi::apimachinery::pkg::apis::meta::v1 as metav1;
use kube_core::metadata::{ObjectMeta, TypeMeta};
use serde::{Deserialize, Serialize};

use super::is_false;
use super::phases::{ConcurrencyPolicy, CronWorkflowPhase};
use super::status::Conditions;
use super::workflow::WorkflowSpec;

/// CronWorkflow is the definition of a scheduled workflow resource.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CronWorkflow {
    #[serde(flatten)]
    pub types: TypeMeta,
    #[serde(default)]
    pub metadata: ObjectMeta,
    /// Spec of the workflow to be run on schedule (always serialized).
    #[serde(default)]
    pub spec: CronWorkflowSpec,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<CronWorkflowStatus>,
}

/// CronWorkflowList is list of CronWorkflow resources.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CronWorkflowList {
    #[serde(flatten)]
    pub types: TypeMeta,
    #[serde(default)]
    pub metadata: metav1::ListMeta,
    #[serde(default)]
    pub items: Vec<CronWorkflow>,
}

/// CronWorkflowSpec is the specification of a CronWorkflow.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CronWorkflowSpec {
    /// WorkflowSpec is the spec of the workflow to be run.
    #[serde(default, rename = "workflowSpec")]
    pub workflow_spec: WorkflowSpec,
    /// ConcurrencyPolicy is the K8s-style concurrency policy
    /// (`Allow`/`Forbid`/`Replace`, see [`super::phases`]).
    #[serde(
        default,
        rename = "concurrencyPolicy",
        skip_serializing_if = "String::is_empty"
    )]
    pub concurrency_policy: ConcurrencyPolicy,
    /// Suspend is a flag that will stop new CronWorkflows from running if true.
    #[serde(default, skip_serializing_if = "is_false")]
    pub suspend: bool,
    /// StartingDeadlineSeconds limits the time a CronWorkflow will be run
    /// after its original scheduled time if it is missed.
    #[serde(
        default,
        rename = "startingDeadlineSeconds",
        skip_serializing_if = "Option::is_none"
    )]
    pub starting_deadline_seconds: Option<i64>,
    /// SuccessfulJobsHistoryLimit is the number of successful jobs kept.
    #[serde(
        default,
        rename = "successfulJobsHistoryLimit",
        skip_serializing_if = "Option::is_none"
    )]
    pub successful_jobs_history_limit: Option<i32>,
    /// FailedJobsHistoryLimit is the number of failed jobs kept.
    #[serde(
        default,
        rename = "failedJobsHistoryLimit",
        skip_serializing_if = "Option::is_none"
    )]
    pub failed_jobs_history_limit: Option<i32>,
    /// Timezone the cron schedule is calculated against, e.g. "Asia/Tokyo";
    /// default is the machine's local time.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub timezone: String,
    /// WorkflowMetadata contains some metadata of the workflow to be run.
    #[serde(
        default,
        rename = "workflowMetadata",
        skip_serializing_if = "Option::is_none"
    )]
    pub workflow_metadata: Option<metav1::ObjectMeta>,
    /// v3.6+: StopStrategy defines if the CronWorkflow should stop
    /// scheduling based on a condition.
    #[serde(
        default,
        rename = "stopStrategy",
        skip_serializing_if = "Option::is_none"
    )]
    pub stop_strategy: Option<StopStrategy>,
    /// v3.6+: Schedules is a list of schedules to run the Workflow in Cron
    /// format. Go has no `omitempty` here, so it is always serialized —
    /// even as `"schedules":[]` when empty.
    ///
    /// v3 compat: the legacy singular `"schedule": "* * * * *"` string is
    /// also accepted (via `alias = "schedule"` + [`de::string_or_vec`], which
    /// takes a bare string or an array of strings). Because `schedules` is
    /// always serialized and `schedule` is only an alias, a legacy input
    /// re-encodes as `"schedules":[...]`.
    #[serde(
        default,
        rename = "schedules",
        alias = "schedule",
        deserialize_with = "de::string_or_vec"
    )]
    pub schedules: Vec<String>,
    /// v3.6+: When is an expression that determines if a run should be
    /// scheduled.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub when: String,
}

/// StopStrategy defines if the CronWorkflow should stop scheduling based on
/// an expression. v3.6+.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct StopStrategy {
    /// Expression stops scheduling workflows when true. Use the variables
    /// `cronworkflow`.`failed` / `cronworkflow`.`succeeded` to access the
    /// number of failed/successful child workflows.
    #[serde(default)]
    pub expression: String,
}

/// CronWorkflowStatus is the status of a CronWorkflow.
///
/// All fields here carry no `omitempty` in Go, so they are always
/// serialized (including `lastScheduledTime: null` when unset, matching the
/// Go pointer-without-omitempty behavior).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CronWorkflowStatus {
    /// Active is a list of active workflows stemming from this CronWorkflow.
    #[serde(default)]
    pub active: Vec<apiv1::ObjectReference>,
    /// LastScheduleTime is the last time the CronWorkflow was scheduled
    /// (serializes `null` when unset, like Go's nil pointer without
    /// omitempty).
    #[serde(default, rename = "lastScheduledTime")]
    pub last_scheduled_time: Option<metav1::Time>,
    /// Conditions is a list of conditions the CronWorkflow may have.
    #[serde(default)]
    pub conditions: Conditions,
    /// v3.6+: Succeeded counts how many times child workflows succeeded.
    #[serde(default)]
    pub succeeded: i64,
    /// v3.6+: Failed counts how many times child workflows failed.
    #[serde(default)]
    pub failed: i64,
    /// v3.6+: Phase is an enum of Active or Stopped; it changes to Stopped
    /// when `stopStrategy.expression` is true (see [`super::phases`]).
    #[serde(default)]
    pub phase: CronWorkflowPhase,
}

/// Serde helpers for `CronWorkflowSpec` v3/v4 schedule compat.
mod de {
    use serde::de::{Error, SeqAccess, Visitor};
    use serde::Deserializer;

    /// Deserialize `Vec<String>` from either a JSON array of strings (v4
    /// `schedules`) or a single JSON string (legacy v3 `schedule`).
    pub fn string_or_vec<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct StringOrVec;
        impl<'de> Visitor<'de> for StringOrVec {
            type Value = Vec<String>;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a cron schedule string or an array of schedule strings")
            }

            fn visit_str<E: Error>(self, v: &str) -> Result<Self::Value, E> {
                Ok(vec![v.to_string()])
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut out = Vec::with_capacity(seq.size_hint().unwrap_or(0));
                while let Some(schedule) = seq.next_element::<String>()? {
                    out.push(schedule);
                }
                Ok(out)
            }
        }
        deserializer.deserialize_any(StringOrVec)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v4_schedules_round_trip() {
        let json = r#"{"apiVersion":"argoproj.io/v1alpha1","kind":"CronWorkflow",
            "metadata":{"name":"c","namespace":"default"},
            "spec":{"workflowSpec":{},"schedules":["* * * * *"],"timezone":"UTC"},
            "status":{"active":[],"succeeded":3,"phase":"Active"}}"#;
        let cwf: CronWorkflow = serde_json::from_str(json).expect("v4 decode");
        assert_eq!(cwf.spec.schedules, vec!["* * * * *"]);
        assert_eq!(cwf.spec.timezone, "UTC");
        assert_eq!(cwf.status.as_ref().unwrap().succeeded, 3);
        let out = serde_json::to_string(&cwf).expect("v4 encode");
        assert!(out.contains(r#""schedules":["* * * * *"]"#), "out={out}");
        assert!(!out.contains(r#""schedule""#), "legacy alias leaked: {out}");
    }

    #[test]
    fn v3_singular_schedule_alias_decodes() {
        let json = r#"{"apiVersion":"argoproj.io/v1alpha1","kind":"CronWorkflow",
            "spec":{"workflowSpec":{},"schedule":"5 * * * *"}}"#;
        let cwf: CronWorkflow = serde_json::from_str(json).expect("v3 decode");
        assert_eq!(cwf.spec.schedules, vec!["5 * * * *"]);
        // Legacy input re-encodes under the v4 key (schedules is always
        // serialized; schedule is alias-only).
        let out = serde_json::to_string(&cwf.spec).expect("re-encode");
        assert!(out.contains(r#""schedules":["5 * * * *"]"#), "out={out}");
    }

    #[test]
    fn status_always_serializes_no_omitempty_fields() {
        let out = serde_json::to_string(&CronWorkflowStatus::default()).expect("enc");
        assert!(out.contains(r#""active":[]"#), "out={out}");
        assert!(out.contains(r#""lastScheduledTime":null"#), "out={out}");
        assert!(out.contains(r#""conditions":[]"#), "out={out}");
        assert!(out.contains(r#""succeeded":0"#), "out={out}");
        assert!(out.contains(r#""failed":0"#), "out={out}");
        assert!(out.contains(r#""phase":"""#), "out={out}");
        // And a spec round-trips its always-serialized schedules even empty.
        let spec = serde_json::to_string(&CronWorkflowSpec::default()).expect("enc");
        assert!(spec.contains(r#""schedules":[]"#), "spec={spec}");
    }
}
