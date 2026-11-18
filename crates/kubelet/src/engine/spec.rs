//! ContainerConfig -> engine ContainerSpec mapping (T4.12/T4.13,
//! Q49/Q50/Q53): the kubelet's CRI shapes flattened onto the engine
//! protocol. Since M4 the image config is the launch source of truth —
//! [`merge_launch`] folds the image's entrypoint/cmd/env/user/workingDir
//! under the CRI request (explicit pod command overrides; pod env
//! appends last-wins) so commandless Pods run straight off their image.

use std::collections::BTreeMap;
use std::path::Path;

use engine_proto::{ContainerSpec, MountSpec};

use super::image::config::{merge_argv, merge_env, ConfigBody};
use super::logs;
use super::registry::Pending;
use runtime::ContainerConfig;

/// The merged launch parameters (T4.13 M4, Q53): the image config's
/// process defaults resolved against one CRI `ContainerConfig`. Built by
/// [`merge_launch`], consumed by [`container_spec`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MergedLaunch {
    pub argv: Vec<String>,
    pub env: Vec<String>,
    pub uid: u32,
    pub gid: u32,
    pub cwd: Option<String>,
}

/// Resolver for named image users: `/etc/passwd` name -> `(uid, gid)`
/// (T4.13 M4, Q53; see `backend::named_user_lookup`).
pub(crate) type NamedUserResolver<'a> = Option<&'a dyn Fn(&str) -> Option<(u32, u32)>>;

/// Fold the image config (when the image came from the CAS) under the
/// CRI container request (T4.13 M4, Q53):
/// - argv: explicit pod `command ++ args` wins outright, else the image's
///   `entrypoint ++ cmd`; still empty means neither side knows what to
///   run — an error.
/// - env: image env first, pod envs appended as `KEY=VALUE` with
///   last-wins dedupe.
/// - user: `""`/absent = root; `"1000"` = uid+gid; `"1000:1000"` splits;
///   a non-numeric name resolves through `resolve_named_user` (the
///   backend feeds it the image's `/etc/passwd`).
/// - cwd: the image `workingDir` (validated absolute; the engine
///   defaults to `/` when unset).
///
/// `img: None` (host-dir images without a CAS record) keeps the
/// B-series contract: an explicit command is still required.
pub(crate) fn merge_launch(
    ccfg: &ContainerConfig,
    img: Option<&ConfigBody>,
    resolve_named_user: NamedUserResolver<'_>,
) -> Result<MergedLaunch, String> {
    let empty = ConfigBody::default();
    let body = img.unwrap_or(&empty);
    let argv = merge_argv(body, &ccfg.command, &ccfg.args);
    if argv.is_empty() {
        return Err("image has no entrypoint/cmd and the pod gives no command".to_string());
    }
    let pod_env: Vec<String> = ccfg
        .envs
        .iter()
        .map(|kv| format!("{}={}", kv.key, kv.value))
        .collect();
    let (uid, gid) = resolve_user(body.user.as_deref(), resolve_named_user)?;
    let cwd = match body.working_dir.as_deref().map(str::trim) {
        Some(dir) if !dir.is_empty() => {
            if !Path::new(dir).is_absolute() {
                return Err(format!("image workingDir {dir:?} is not absolute"));
            }
            Some(dir.to_string())
        }
        _ => None,
    };
    Ok(MergedLaunch {
        argv,
        env: merge_env(&body.env, &pod_env),
        uid,
        gid,
        cwd,
    })
}

/// Resolve the image `User` string to `(uid, gid)` (T4.13 M4, Q53).
fn resolve_user(
    user: Option<&str>,
    resolve_named_user: NamedUserResolver<'_>,
) -> Result<(u32, u32), String> {
    let unresolvable = |user: &str| {
        format!(
            "image user {user:?} cannot be resolved (numeric uid[:gid] or \
             resolvable /etc/passwd name required)"
        )
    };
    let Some(user) = user.map(str::trim).filter(|u| !u.is_empty()) else {
        return Ok((0, 0));
    };
    let (uid_part, gid_part) = match user.split_once(':') {
        Some((u, g)) => (u, Some(g)),
        None => (user, None),
    };
    if let Ok(uid) = uid_part.parse::<u32>() {
        let gid = match gid_part {
            // "1000" keeps the uid as the gid (Docker convention).
            None => uid,
            Some(g) => g.parse::<u32>().map_err(|_| unresolvable(user))?,
        };
        return Ok((uid, gid));
    }
    // A named user: no resolver or no match is a hard error (never
    // silently run as root).
    resolve_named_user
        .and_then(|resolve| resolve(uid_part))
        .ok_or_else(|| unresolvable(user))
}

/// Pure `/etc/passwd` parser (T4.13 M4, Q53): `name:pw:uid:gid:...`
/// lines become `name -> (uid, gid)`; malformed lines are skipped.
pub(crate) fn parse_passwd_uids(content: &str) -> BTreeMap<String, (u32, u32)> {
    let mut map = BTreeMap::new();
    for line in content.lines() {
        let mut fields = line.split(':');
        let parsed = (fields.next(), fields.next(), fields.next(), fields.next());
        if let (Some(name), Some(_pw), Some(uid), Some(gid)) = parsed {
            if !name.is_empty() {
                if let (Ok(uid), Ok(gid)) = (uid.parse::<u32>(), gid.parse::<u32>()) {
                    map.insert(name.to_string(), (uid, gid));
                }
            }
        }
    }
    map
}

/// Build the engine spec for a created container. The launch half of the
/// spec comes from [`MergedLaunch`] (image config merged under the CRI
/// request, T4.13 M4); everything else is the plain T4.12 mapping.
pub(crate) fn container_spec(
    id: &str,
    rootfs: &Path,
    pending: &Pending,
    sandbox_netns: String,
    launch: MergedLaunch,
) -> Result<ContainerSpec, String> {
    let log = logs::log_file(&pending.pcfg.log_directory, &pending.ccfg.log_path);
    logs::ensure_parents(&log).map_err(|e| format!("create log dir {}: {e}", log.display()))?;
    let r = &pending.ccfg.linux.resources;
    Ok(ContainerSpec {
        id: id.to_string(),
        rootfs: rootfs.to_string_lossy().into_owned(),
        argv: launch.argv,
        env: launch.env,
        hostname: Some(pending.pcfg.hostname.clone()).filter(|h| !h.is_empty()),
        cwd: launch.cwd.unwrap_or_else(|| "/".into()),
        uid: launch.uid,
        gid: launch.gid,
        mounts: pending
            .ccfg
            .mounts
            .iter()
            .map(|m| MountSpec {
                source: m.host_path.clone(),
                target: m.container_path.clone(),
                readonly: m.readonly,
            })
            .collect(),
        new_net: false,
        netns: Some(sandbox_netns),
        // T4.13 M3 (Q53): set by the backend when the image resolves to a
        // per-container overlayfs view (plain dirs keep None).
        overlay: None,
        pause: false,
        log_path: Some(log.to_string_lossy().into_owned()),
        no_new_privs: true,
        cgroup_name: None,
        memory_max: (r.memory_limit_in_bytes > 0).then_some(r.memory_limit_in_bytes as u64),
        cpu_max: (r.cpu_quota > 0 && r.cpu_period > 0)
            .then(|| format!("{} {}", r.cpu_quota, r.cpu_period)),
        pids_max: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use runtime::cri_json::{ContainerMetadata, ImageSpec, KeyValue, SandboxMetadata};
    use runtime::{ContainerConfig, PodSandboxConfig};

    fn pending(command: Vec<String>) -> Pending {
        // Only pair args with a real command: the commandless case must
        // have neither.
        let args = if command.is_empty() {
            vec![]
        } else {
            vec!["-v".into()]
        };
        Pending {
            ccfg: ContainerConfig {
                metadata: ContainerMetadata {
                    name: "web".into(),
                    attempt: 0,
                },
                image: ImageSpec {
                    image: "/tmp/rootfs".into(),
                },
                command,
                args,
                // sync.rs always fills this: <container>.<attempt>.log.
                log_path: "web.0.log".into(),
                ..Default::default()
            },
            pcfg: PodSandboxConfig {
                metadata: SandboxMetadata {
                    name: "p".into(),
                    namespace: "default".into(),
                    uid: "u".into(),
                    attempt: 0,
                },
                hostname: "p".into(),
                log_directory: "/tmp/logs/pod".into(),
                ..Default::default()
            },
            sandbox_id: "sb-1".into(),
        }
    }

    fn ccfg(command: &[&str], envs: &[(&str, &str)]) -> ContainerConfig {
        ContainerConfig {
            metadata: ContainerMetadata {
                name: "c".into(),
                attempt: 0,
            },
            image: ImageSpec {
                image: "test/img:v1".into(),
            },
            command: command.iter().map(|s| s.to_string()).collect(),
            envs: envs
                .iter()
                .map(|(k, v)| KeyValue {
                    key: k.to_string(),
                    value: v.to_string(),
                })
                .collect(),
            ..Default::default()
        }
    }

    fn body(user: Option<&str>, env: &[&str], entry: &[&str], cmd: &[&str]) -> ConfigBody {
        ConfigBody {
            user: user.map(str::to_string),
            env: env.iter().map(|s| s.to_string()).collect(),
            entrypoint: entry.iter().map(|s| s.to_string()).collect(),
            cmd: cmd.iter().map(|s| s.to_string()).collect(),
            working_dir: None,
        }
    }

    #[test]
    fn maps_the_cri_pair_onto_the_engine_spec() {
        let p = pending(vec!["/bin/echo".into()]);
        let launch = merge_launch(&p.ccfg, None, None).expect("launch");
        let spec = container_spec(
            "c-1",
            Path::new("/tmp/rootfs"),
            &p,
            "/proc/9/ns/net".into(),
            launch,
        )
        .expect("spec");
        assert_eq!(spec.argv, vec!["/bin/echo", "-v"]);
        assert_eq!(spec.netns.as_deref(), Some("/proc/9/ns/net"));
        assert_eq!(spec.log_path.as_deref(), Some("/tmp/logs/pod/web.0.log"));
        assert_eq!(spec.hostname.as_deref(), Some("p"));
        assert_eq!((spec.uid, spec.gid), (0, 0));
        assert_eq!(spec.cwd, "/");
        assert!(!spec.new_net && !spec.pause);
    }

    #[test]
    fn explicit_command_overrides_the_image_entrypoint() {
        let cfg = ccfg(&["/usr/bin/env", "A=1"], &[]);
        let img = body(Some("1000"), &[], &["/bin/sh"], &["-c", "sleep 1"]);
        let launch = merge_launch(&cfg, Some(&img), None).expect("launch");
        assert_eq!(launch.argv, vec!["/usr/bin/env", "A=1"]);
    }

    #[test]
    fn image_entrypoint_and_cmd_run_without_a_pod_command() {
        let cfg = ccfg(&[], &[]);
        let img = body(
            None,
            &[],
            &["/docker-entrypoint.sh"],
            &["nginx", "-g", "daemon off;"],
        );
        let launch = merge_launch(&cfg, Some(&img), None).expect("launch");
        assert_eq!(
            launch.argv,
            vec!["/docker-entrypoint.sh", "nginx", "-g", "daemon off;"]
        );
    }

    #[test]
    fn neither_image_nor_pod_command_is_an_error() {
        let cfg = ccfg(&[], &[]);
        assert!(merge_launch(&cfg, None, None).is_err());
        let img = body(None, &[], &[], &[]);
        assert!(merge_launch(&cfg, Some(&img), None).is_err());
    }

    #[test]
    fn env_appends_pod_after_image_last_wins() {
        let cfg = ccfg(&["/bin/true"], &[("B", "3"), ("C", "4")]);
        let img = body(None, &["A=1", "B=2", "A=2"], &["/bin/true"], &[]);
        let launch = merge_launch(&cfg, Some(&img), None).expect("launch");
        assert_eq!(launch.env, vec!["A=2", "B=3", "C=4"]);
        // Without an image the pod envs still land verbatim.
        let bare = merge_launch(&cfg, None, None).expect("launch");
        assert_eq!(bare.env, vec!["B=3", "C=4"]);
    }

    #[test]
    fn user_numeric_forms_and_named_resolution() {
        let cfg = ccfg(&["/bin/true"], &[]);
        // "1000" -> uid and gid; "1000:1000" splits.
        let uid_only = body(Some("1000"), &[], &["/bin/true"], &[]);
        assert_eq!(
            merge_launch(&cfg, Some(&uid_only), None).expect("launch"),
            MergedLaunch {
                argv: vec!["/bin/true".into()],
                env: vec![],
                uid: 1000,
                gid: 1000,
                cwd: None,
            }
        );
        let pair = body(Some("1000:150"), &[], &["/bin/true"], &[]);
        let launch = merge_launch(&cfg, Some(&pair), None).expect("launch");
        assert_eq!((launch.uid, launch.gid), (1000, 150));
        // A named user resolves through the injected /etc/passwd lookup.
        let named = body(Some("app"), &[], &["/bin/true"], &[]);
        let resolve = |name: &str| (name == "app").then_some((2000u32, 2500u32));
        let launch = merge_launch(&cfg, Some(&named), Some(&resolve)).expect("launch");
        assert_eq!((launch.uid, launch.gid), (2000, 2500));
        // No resolver / no match: a hard error, never silent root.
        let err = merge_launch(&cfg, Some(&named), None).unwrap_err();
        assert!(err.contains("cannot be resolved"), "{err}");
        let err = merge_launch(&cfg, Some(&named), Some(&|_: &str| None)).unwrap_err();
        assert!(err.contains("cannot be resolved"), "{err}");
        // Absent/empty user stays root.
        let root = body(Some(""), &[], &["/bin/true"], &[]);
        let launch = merge_launch(&cfg, Some(&root), None).expect("launch");
        assert_eq!((launch.uid, launch.gid), (0, 0));
    }

    #[test]
    fn working_dir_must_be_absolute_when_set() {
        let cfg = ccfg(&["/bin/true"], &[]);
        let mut img = body(None, &[], &["/bin/true"], &[]);
        img.working_dir = Some("/srv/app".into());
        let launch = merge_launch(&cfg, Some(&img), None).expect("launch");
        assert_eq!(launch.cwd.as_deref(), Some("/srv/app"));
        img.working_dir = Some("srv/relative".into());
        assert!(merge_launch(&cfg, Some(&img), None).is_err());
        img.working_dir = Some("".into());
        assert_eq!(
            merge_launch(&cfg, Some(&img), None).expect("launch").cwd,
            None
        );
    }

    #[test]
    fn passwd_parser_maps_names_and_skips_malformed_lines() {
        let content = "root:x:0:0:root:/root:/bin/sh\n\
                       app:x:1000:1500:App User:/home/app:/bin/sh\n\
                       malformed-line-without-fields\n\
                       bad:x:notanum:1500:Bad:/home/bad:/bin/false\n";
        let map = parse_passwd_uids(content);
        assert_eq!(map.len(), 2);
        assert_eq!(map.get("root"), Some(&(0, 0)));
        assert_eq!(map.get("app"), Some(&(1000, 1500)));
        assert!(!map.contains_key("bad"));
        assert!(parse_passwd_uids("").is_empty());
    }
}
