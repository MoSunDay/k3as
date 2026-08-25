//! Argo Workflows synchronization (semaphore/mutex) status types
//! (TODO **T7.3 Scope A**, decision **Q30**).
//!
//! 1:1 typed, data-only port of upstream `workflow_types.go` v4
//! (~4200-4427): lock holding/waiting bookkeeping for workflow and node
//! synchronization. The Go `SynchronizationAction` interface and its
//! `Lock*`/`Get*` methods are intentionally NOT ported (behavior, not wire
//! data). Field-level SSOT is the Go json tags; JSON-only wire (Q10).

use serde::{Deserialize, Serialize};

use super::is_false;

/// SemaphoreHolding describes the semaphore and the holders currently
/// holding it.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SemaphoreHolding {
    /// Semaphore stores the semaphore name.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub semaphore: String,
    /// Holders stores the list of current holder names in the workflow.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub holders: Vec<String>,
}

/// SemaphoreStatus contains the semaphores this workflow holds and the ones
/// it is waiting on.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SemaphoreStatus {
    /// Holding stores the list of resource acquired synchronization locks
    /// for the workflow.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub holding: Vec<SemaphoreHolding>,
    /// Waiting indicates the list of current synchronization lock holders
    /// this workflow is waiting on.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub waiting: Vec<SemaphoreHolding>,
}

/// MutexHolding describes the mutex and the object which is holding it.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MutexHolding {
    /// Reference for the mutex, e.g. `${namespace}/mutex/${mutexName}`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub mutex: String,
    /// Holder is a reference to the object which holds the mutex: the
    /// current workflow's NodeID when holding, or
    /// `${WorkflowName}/${NodeID}` when waiting on another workflow.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub holder: String,
}

/// MutexStatus contains which objects hold mutex locks, and which objects
/// this workflow is waiting on to release locks.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MutexStatus {
    /// Holding is a list of mutexes and their respective objects that are
    /// held by mutex lock for this workflow.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub holding: Vec<MutexHolding>,
    /// Waiting is a list of mutexes and their respective objects this
    /// workflow is waiting for.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub waiting: Vec<MutexHolding>,
}

/// SynchronizationStatus stores the semaphore/mutex lock status of a
/// workflow.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SynchronizationStatus {
    /// Semaphore stores this workflow's semaphore holder details.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub semaphore: Option<SemaphoreStatus>,
    /// Mutex stores this workflow's mutex holder details.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mutex: Option<MutexStatus>,
}

/// NodeSynchronizationStatus stores the status of a node's synchronization.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct NodeSynchronizationStatus {
    /// Waiting is the name of the lock that this node is waiting for.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub waiting: String,
}

/// NodeFlag tracks flags on a node.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct NodeFlag {
    /// Hooked tracks whether or not this node was triggered by a hook or
    /// onExit.
    #[serde(default, skip_serializing_if = "is_false")]
    pub hooked: bool,
    /// Retried tracks whether or not this node was retried by
    /// retryStrategy.
    #[serde(default, skip_serializing_if = "is_false")]
    pub retried: bool,
}

/// TemplateAnnotation is the type of annotations on templates
/// (Go: `type TemplateAnnotation string`).
pub type TemplateAnnotation = String;

/// Display-name annotation key carried on templates.
pub const TEMPLATE_ANNOTATION_DISPLAY_NAME: &str = "workflows.argoproj.io/display-name";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semaphore_status_round_trip() {
        let status: SemaphoreStatus = serde_json::from_str(
            r#"{"holding":[{"semaphore":"s","holders":["w-1"]}],"waiting":[{"semaphore":"s2"}]}"#,
        )
        .unwrap();
        assert_eq!(status.holding.len(), 1);
        assert_eq!(status.holding[0].semaphore, "s");
        assert_eq!(status.holding[0].holders, vec!["w-1".to_string()]);
        let out = serde_json::to_value(&status).unwrap();
        assert_eq!(out["holding"][0]["semaphore"], "s");
        assert_eq!(out["waiting"][0]["semaphore"], "s2");
        // omitempty: empty holders list stays absent.
        assert!(out["waiting"][0].get("holders").is_none());
    }

    #[test]
    fn synchronization_status_optional_locks() {
        let status: SynchronizationStatus = serde_json::from_str(r#"{}"#).unwrap();
        assert!(status.semaphore.is_none() && status.mutex.is_none());
        let out = serde_json::to_string(&status).unwrap();
        assert_eq!(out, "{}");

        let with_mutex: SynchronizationStatus =
            serde_json::from_str(r#"{"mutex":{"holding":[{"mutex":"m","holder":"n1"}]}}"#).unwrap();
        assert!(with_mutex.semaphore.is_none());
        assert_eq!(with_mutex.mutex.as_ref().unwrap().holding[0].holder, "n1");
    }

    #[test]
    fn node_flag_omitempty() {
        let flag: NodeFlag = serde_json::from_str(r#"{"hooked":true}"#).unwrap();
        assert!(flag.hooked && !flag.retried);
        assert_eq!(serde_json::to_string(&flag).unwrap(), r#"{"hooked":true}"#);
        assert_eq!(serde_json::to_string(&NodeFlag::default()).unwrap(), "{}");
    }
}
