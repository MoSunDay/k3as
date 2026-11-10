//! Workspace path conventions (T-anyhome, decision Q36; per-pod mounts
//! T7.5, decision Q43): the SSOT for where anyhome exposes workflow and
//! agent memory and where containers mount it. Shared by the anyhome
//! workspace keeper (server), the Workflow controller's /workspace
//! auto-mount injection, Phase B agent execution, and the apiserver's
//! per-Pod anyhome defaulting (T7.5).
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

// ---- per-Pod anyhome mounts (T7.5, Q43) -----------------------------------

/// The per-Pod workspace annotation key (T7.5, Q43): `init-pro.io/anyhome`
/// with value `rw` | `ro` | `true` | `false` (`true` == `rw`; `false` opts
/// the pod out). When enabled, the apiserver defaults the pod with one
/// generic nfs volume + mount carrying the pod's own anyhome workspace
/// ([`pod_workspace_path`]) at [`MOUNT_TARGET`]; `ro` maps to volume
/// `readOnly: true` -- CLIENT-enforced (CRI readonly bind), the same trust
/// model as a native k8s NFS readOnly (server-side per-path ro would need
/// vfs capability work in the pinned nfsserve and is out of scope).
pub const ANYHOME_ANNOTATION: &str = "init-pro.io/anyhome";

/// The extra-mounts annotation key (T7.5, Q43): comma-separated
/// `<server-path>:<container-path>[:ro|:rw]` triples -- each parsed triple
/// becomes ONE injected nfs volume + volumeMount (see
/// [`parse_anyhome_mounts`]).
pub const ANYHOME_MOUNTS_ANNOTATION: &str = "init-pro.io/anyhome-mounts";

/// The injected workspace volume name (T7.5, Q43). A pod-level volume with
/// this literal name (or an nfs volume with the same server path) suppresses
/// injection -- the A4 user-declaration-wins rule, the same shape as the
/// Workflow engine's `workspace` volume (`user_declares_workspace`).
pub const ANYHOME_VOLUME: &str = "anyhome";

/// Per-pod workspace layout root (T7.5, Q43): `/pod/<namespace>/<name>`.
/// The extra `pod/` segment keeps the layout DISJOINT from `/workflow/<id>`
/// and `/agent/<id>` -- a namespace may legally be called `workflow`, so a
/// literal `/<ns>/<name>` layout could collide with those roots.
pub const POD_WORKSPACE_ROOT: &str = "/pod";

/// The anyhome export path holding one Pod's own workspace (T7.5, Q43):
/// `/pod/{namespace}/{name}`. `None` when either part is empty or contains
/// `/` (see [`valid_workspace_id`]); both parts are k8s metadata names.
pub fn pod_workspace_path(namespace: &str, name: &str) -> Option<String> {
    (valid_workspace_id(namespace) && valid_workspace_id(name))
        .then(|| format!("{POD_WORKSPACE_ROOT}/{namespace}/{name}"))
}

/// Parsed `init-pro.io/anyhome` mode (T7.5, Q43). `Off` = the annotation is
/// present but explicitly disabled (`"false"`): no workspace injection, no
/// error -- the extra-mounts annotation stays independently in effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnyhomeMode {
    /// `"rw"` / `"true"`: read-write workspace at [`MOUNT_TARGET`].
    Rw,
    /// `"ro"`: same, but the injected volume carries `readOnly: true`.
    Ro,
    /// `"false"`: opted out.
    Off,
}

/// Parse the `init-pro.io/anyhome` annotation value (T7.5, Q43). Exact,
/// case-sensitive spellings only (annotations are precise wire data):
/// `rw`/`true` -> [`AnyhomeMode::Rw`], `ro` -> [`AnyhomeMode::Ro`],
/// `false` -> [`AnyhomeMode::Off`]. Anything else is `None` -- the caller
/// (apiserver defaulting) rejects the create; a legacy/direct-store object
/// with a bad value is simply skipped by the keeper.
pub fn parse_anyhome_mode(value: &str) -> Option<AnyhomeMode> {
    match value {
        "rw" | "true" => Some(AnyhomeMode::Rw),
        "ro" => Some(AnyhomeMode::Ro),
        "false" => Some(AnyhomeMode::Off),
        _ => None,
    }
}

/// One parsed `init-pro.io/anyhome-mounts` triple (T7.5, Q43).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnyhomeMount {
    /// Absolute anyhome export path, e.g. `/team/shared` (global, NOT
    /// namespaced -- shared memory by design).
    pub server_path: String,
    /// Absolute in-container mount path, e.g. `/shared`.
    pub container_path: String,
    /// `:ro` -> true; `:rw` or absent -> false.
    pub read_only: bool,
}

/// Parse the `init-pro.io/anyhome-mounts` annotation value (T7.5, Q43):
/// comma-separated `<server-path>:<container-path>[:ro|:rw]`. Pure and
/// total: every segment must carry two ABSOLUTE paths (no empty, `.` or
/// `..` path segments -- the guard below), the optional third field must be
/// exactly `ro` or `rw`, and the whole annotation de-duplicates by
/// CONTAINER path (first wins; a later triple targeting an
/// already-taken mount path is dropped, not an error). An empty value is
/// `Ok(vec![])` (inert). The first offender names itself in the `Err`.
pub fn parse_anyhome_mounts(value: &str) -> Result<Vec<AnyhomeMount>, String> {
    if value.is_empty() {
        return Ok(Vec::new());
    }
    let mut mounts: Vec<AnyhomeMount> = Vec::new();
    for raw in value.split(',') {
        let segment = raw.trim();
        if segment.is_empty() {
            return Err("empty mount segment (no ',,' allowed)".to_string());
        }
        let parts: Vec<&str> = segment.split(':').collect();
        let (server, container, mode) = match parts.as_slice() {
            [s, c] => (s, c, None),
            [s, c, m] => (s, c, Some(*m)),
            _ => {
                return Err(format!(
                    "segment \"{segment}\": expected <server-path>:<container-path>[:ro|:rw]"
                ))
            }
        };
        if !valid_export_path(server) {
            return Err(format!(
                "segment \"{segment}\": server path \"{server}\" must be absolute with no '.', '..' or empty path segments"
            ));
        }
        if !valid_export_path(container) {
            return Err(format!(
                "segment \"{segment}\": container path \"{container}\" must be absolute with no '.', '..' or empty path segments"
            ));
        }
        let read_only = match mode {
            None | Some("rw") => false,
            Some("ro") => true,
            Some(other) => {
                return Err(format!(
                    "segment \"{segment}\": mode field must be 'ro' or 'rw', got '{other}'"
                ))
            }
        };
        if mounts.iter().any(|m| m.container_path == *container) {
            continue; // first wins: later triple at the same mount path drops
        }
        mounts.push(AnyhomeMount {
            server_path: server.to_string(),
            container_path: container.to_string(),
            read_only,
        });
    }
    Ok(mounts)
}

/// Absolute-path guard for `anyhome-mounts` halves (T7.5, Q43): must start
/// with `/` and carry no `.`, `..` or empty path segments -- stricter than
/// [`valid_workspace_id`] because these are FREE-FORM user paths, not k8s
/// metadata names, so `.` cannot be waved through here.
fn valid_export_path(path: &str) -> bool {
    let Some(rest) = path.strip_prefix('/') else {
        return false;
    };
    !rest.is_empty()
        && rest
            .split('/')
            .all(|seg| !seg.is_empty() && seg != "." && seg != "..")
}

/// Deterministic injected volume name for one extra mount (T7.5, Q43):
/// `anyhome-<sanitized container path>` -- lowercased, non
/// `[a-z0-9-]` chars collapsed to `-`, leading `/` dropped, capped at 63
/// chars (a DNS_LABEL, like every k8s volume name). Deterministic FROM the
/// annotation so repeated applies derive the same name (idempotent
/// injection); a user volume with this literal name suppresses injection
/// (A4).
pub fn anyhome_mount_volume_name(container_path: &str) -> String {
    let sanitized: String = container_path
        .trim_start_matches('/')
        .chars()
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' {
                c
            } else if c.is_ascii_uppercase() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let base = if sanitized.is_empty() {
        "mount"
    } else {
        sanitized.as_str()
    };
    let name = format!("{ANYHOME_VOLUME}-{base}");
    name.chars().take(63).collect()
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
        assert_eq!(agent_workspace_path(".."), Some("/agent/..".to_string()));
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
    fn pod_workspace_path_happy_path() {
        assert_eq!(
            pod_workspace_path("default", "p-1"),
            Some("/pod/default/p-1".to_string())
        );
        assert_eq!(
            pod_workspace_path("team-a", "web-abc"),
            Some("/pod/team-a/web-abc".to_string())
        );
    }

    #[test]
    fn pod_workspace_path_rejects_bad_parts() {
        assert_eq!(pod_workspace_path("", "p"), None);
        assert_eq!(pod_workspace_path("ns", ""), None);
        // `workflow` is a LEGAL namespace: the /pod root keeps it disjoint
        // from /workflow/<id> (the reason the literal /<ns>/<name> layout
        // was rejected, Q43).
        assert_eq!(
            pod_workspace_path("workflow", "wf"),
            Some("/pod/workflow/wf".to_string())
        );
        assert_eq!(pod_workspace_path("a/b", "p"), None);
        assert_eq!(pod_workspace_path("ns", "p/q"), None);
    }

    #[test]
    fn anyhome_mode_spellings() {
        assert_eq!(parse_anyhome_mode("rw"), Some(AnyhomeMode::Rw));
        assert_eq!(parse_anyhome_mode("true"), Some(AnyhomeMode::Rw));
        assert_eq!(parse_anyhome_mode("ro"), Some(AnyhomeMode::Ro));
        assert_eq!(parse_anyhome_mode("false"), Some(AnyhomeMode::Off));
        for bad in ["banana", "", "RW", "True", "read-only"] {
            assert_eq!(parse_anyhome_mode(bad), None, "mode {bad:?}");
        }
    }

    #[test]
    fn mounts_happy_path() {
        let mounts = parse_anyhome_mounts("/team/shared:/shared:ro,/cache:/cache").unwrap();
        assert_eq!(
            mounts,
            vec![
                AnyhomeMount {
                    server_path: "/team/shared".into(),
                    container_path: "/shared".into(),
                    read_only: true,
                },
                AnyhomeMount {
                    server_path: "/cache".into(),
                    container_path: "/cache".into(),
                    read_only: false,
                },
            ]
        );
        // Explicit :rw == absent mode.
        assert!(!parse_anyhome_mounts("/a:/a:rw").unwrap()[0].read_only);
    }

    #[test]
    fn mounts_empty_value_is_inert() {
        assert_eq!(parse_anyhome_mounts("").unwrap(), Vec::new());
    }

    #[test]
    fn mounts_reject_relative_and_traversal() {
        for bad in [
            "team/shared:/s", // relative server path
            "/s:s",           // relative container path
            "/../etc:/e",     // .. server segment
            "/s:/../e",       // .. container segment
            "/a/./b:/s",      // . server segment
            "/a//b:/s",       // empty server segment
            "/a:/s//x",       // empty container segment
            "/a:/s:banana",   // bad mode field
            "/a",             // missing container half
            "/a:/s:/x",       // four fields
            "/a:/s,",         // empty trailing segment
        ] {
            assert!(parse_anyhome_mounts(bad).is_err(), "value {bad:?}");
        }
    }

    #[test]
    fn mounts_dedupe_by_container_path_first_wins() {
        // A malformed segment is REJECTED before dedup ever runs -- errors
        // beat silently dropping.
        assert!(parse_anyhome_mounts("/one:/x:ro,/two:/x:/x:/nope").is_err());
        // Well-formed duplicate mount path: first wins, no error.
        let mounts = parse_anyhome_mounts("/one:/x:ro,/two:/x").unwrap();
        assert_eq!(mounts.len(), 1);
        assert_eq!(mounts[0].server_path, "/one");
        // Same server path at two mount paths is fine (two volumes).
        let mounts = parse_anyhome_mounts("/s:/a,/s:/b").unwrap();
        assert_eq!(mounts.len(), 2);
    }

    #[test]
    fn mount_volume_names_are_deterministic_dns_labels() {
        assert_eq!(anyhome_mount_volume_name("/shared"), "anyhome-shared");
        assert_eq!(
            anyhome_mount_volume_name("/team/Shared_Data"),
            "anyhome-team-shared-data"
        );
        let long = format!("/{}", "x".repeat(200));
        let name = anyhome_mount_volume_name(&long);
        assert_eq!(name.len(), 63);
        assert!(name.starts_with("anyhome-"));
        // ASCII-only mapping: slicing at 63 bytes cannot split a char.
        assert!(name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'));
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
