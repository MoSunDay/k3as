//! Workspace path conventions (T-anyhome, decision Q36): the SSOT for
//! where anyhome exposes workflow and agent memory and where containers
//! mount it. Shared by the anyhome workspace keeper (server), the
//! Workflow controller's /workspace auto-mount injection, and Phase B
//! agent execution.
//!
//! Lives in `common` (not `agent`) because `agent` transitively drags
//! opencoder's libsql into every binary that embeds anyhome's -- duplicate
//! libsql C symbols fail the final link (T-anyhome finding).
//!
//! Pure module: string building only -- no I/O, no environment reads, no
//! state. The constants/functions below are the single place allowed to
//! spell these paths, so the keeper, the controller, and the kubelet mount
//! injection cannot drift apart.

/// The in-container mount point every workspace is injected at (T-anyhome,
/// Q36). A container sees exactly one workspace, mounted here regardless of
/// whether it backs an Agent (memory) or a Workflow (shared scratch space),
/// so tooling inside the container never needs the backing id.
pub const MOUNT_TARGET: &str = "/workspace";

/// The workspace contract annotation key (T-anyhome, Q36): stamped by the
/// Workflow controller on Agent children (`build_agent_with`) and consumed
/// by the node-side Agent runner (T7.1 Phase B, Q37). Value shape is
/// [`workspace_nfs_url`] output: `nfs://<addr>/agent/<name>`. Defined HERE
/// (the SSOT) so the producer (controllers) and the consumer (kubelet)
/// cannot drift -- kubelet cannot depend on controllers.
pub const WORKSPACE_ANNOTATION: &str = "init-pro.io/workspace";

/// Path-safety guard shared by [`agent_workspace_path`] and
/// [`workflow_workspace_path`] (T-anyhome, Q36).
///
/// `/` is the only rejected character: it is the sole path-significant
/// separator on POSIX, so it is the only way an id could escape the
/// `/agent/` or `/workflow/` prefix (or introduce a `../` climb, which needs
/// a separator to work). `..` alone is deliberately ALLOWED to pass: ids are
/// Kubernetes `metadata.name`, an RFC1123 subdomain (lowercase alnum, `-`,
/// `.`), so `.` is a legal in-name character and the apiserver already
/// rejects names that are not valid subdomains; a bare `..` inside
/// `/agent/..` is a normalized-away segment, not an escape, and refusing it
/// here would duplicate upstream validation rather than guard a real path
/// boundary.
fn valid_workspace_id(id: &str) -> bool {
    !id.is_empty() && !id.contains('/')
}

/// The anyhome export path holding one Agent's memory (T-anyhome, Q36):
/// `/agent/{agent_id}`. `None` when `agent_id` is empty or contains `/`
/// (see [`valid_workspace_id`]).
pub fn agent_workspace_path(agent_id: &str) -> Option<String> {
    if valid_workspace_id(agent_id) {
        Some(format!("/agent/{agent_id}"))
    } else {
        None
    }
}

/// The anyhome export path holding one Workflow's shared workspace
/// (T-anyhome, Q36): `/workflow/{workflow_id}`. `None` when `workflow_id`
/// is empty or contains `/` (see [`valid_workspace_id`]).
pub fn workflow_workspace_path(workflow_id: &str) -> Option<String> {
    if valid_workspace_id(workflow_id) {
        Some(format!("/workflow/{workflow_id}"))
    } else {
        None
    }
}

/// The annotation/contract URL shape pointing a workspace at an anyhome NFS
/// endpoint (T-anyhome, Q36): `nfs://{addr}{path}`. `addr` carries host and
/// port; `path` already starts with `/` (it is a [`agent_workspace_path`] /
/// [`workflow_workspace_path`] result).
pub fn workspace_nfs_url(addr: &str, path: &str) -> String {
    format!("nfs://{addr}{path}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mount_target_is_the_single_in_container_path() {
        assert_eq!(MOUNT_TARGET, "/workspace");
        assert!(MOUNT_TARGET.starts_with('/'), "absolute path");
        assert!(!MOUNT_TARGET.ends_with('/'), "no trailing slash");
    }

    #[test]
    fn workspace_annotation_key_is_stable() {
        // Q37: the runner parses exactly this key off Agent metadata.
        assert_eq!(WORKSPACE_ANNOTATION, "init-pro.io/workspace");
        assert!(WORKSPACE_ANNOTATION.starts_with("init-pro.io/"));
    }

    #[test]
    fn agent_workspace_path_happy_path() {
        assert_eq!(
            agent_workspace_path("agent-1"),
            Some("/agent/agent-1".to_string())
        );
        assert_eq!(
            agent_workspace_path("demo-agent"),
            Some("/agent/demo-agent".to_string())
        );
    }

    #[test]
    fn workflow_workspace_path_happy_path() {
        assert_eq!(
            workflow_workspace_path("wf-1"),
            Some("/workflow/wf-1".to_string())
        );
        assert_eq!(
            workflow_workspace_path("hello-workflow"),
            Some("/workflow/hello-workflow".to_string())
        );
    }

    #[test]
    fn empty_id_is_rejected() {
        assert_eq!(agent_workspace_path(""), None);
        assert_eq!(workflow_workspace_path(""), None);
    }

    #[test]
    fn id_with_separator_is_rejected() {
        // `/` would let an id escape the /agent or /workflow prefix (or forge
        // a `../` climb): the one character the guard must refuse.
        assert_eq!(agent_workspace_path("a/b"), None);
        assert_eq!(agent_workspace_path("/etc"), None);
        assert_eq!(agent_workspace_path("../../etc/passwd"), None);
        assert_eq!(workflow_workspace_path("wf/secret"), None);
        assert_eq!(workflow_workspace_path("../workflow"), None);
    }

    #[test]
    fn dot_dot_id_is_allowed_by_design() {
        // Documented guard scope (Q36): ids are k8s metadata.name (RFC1123
        // subdomain: lowercase alnum, '-', '.'), so '.' is a legal in-name
        // character and '/' -- not '.' -- is the only path-significant char.
        // A separator-free id cannot climb out of /agent or /workflow; the
        // apiserver owns name-shape validation.
        assert_eq!(
            agent_workspace_path(".."),
            Some("/agent/..".to_string())
        );
        assert_eq!(
            workflow_workspace_path("v1.2.3"),
            Some("/workflow/v1.2.3".to_string())
        );
    }

    #[test]
    fn nfs_url_shape() {
        assert_eq!(
            workspace_nfs_url("10.0.0.5:2049", "/agent/agent-1"),
            "nfs://10.0.0.5:2049/agent/agent-1"
        );
        assert_eq!(
            workspace_nfs_url("anyhome.default.svc:2049", "/workflow/wf-1"),
            "nfs://anyhome.default.svc:2049/workflow/wf-1"
        );
    }

    #[test]
    fn nfs_url_composes_with_workspace_paths() {
        let path = agent_workspace_path("agent-1").expect("valid id");
        assert_eq!(
            workspace_nfs_url("127.0.0.1:2049", &path),
            "nfs://127.0.0.1:2049/agent/agent-1"
        );
    }
}
