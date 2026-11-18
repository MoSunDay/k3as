//! ContainerConfig -> engine ContainerSpec mapping (T4.12, Q49/Q50):
//! the kubelet's CRI shapes flattened onto the engine protocol.

use std::path::Path;

use engine_proto::{ContainerSpec, MountSpec};

use super::logs;
use super::registry::Pending;

/// Build the engine spec for a created container. Fails when the pod
/// gives no explicit command (image entrypoint/cmd arrive with the
/// T4.13 image store).
pub(crate) fn container_spec(
    id: &str,
    rootfs: &Path,
    pending: &Pending,
    sandbox_netns: String,
) -> Result<ContainerSpec, String> {
    let mut argv = pending.ccfg.command.clone();
    argv.extend(pending.ccfg.args.iter().cloned());
    if argv.is_empty() {
        return Err(format!(
            "builtin engine needs an explicit command until T4.13 image configs ({id})"
        ));
    }
    let log = logs::log_file(&pending.pcfg.log_directory, &pending.ccfg.log_path);
    logs::ensure_parents(&log).map_err(|e| format!("create log dir {}: {e}", log.display()))?;
    let r = &pending.ccfg.linux.resources;
    Ok(ContainerSpec {
        id: id.to_string(),
        rootfs: rootfs.to_string_lossy().into_owned(),
        argv,
        env: pending
            .ccfg
            .envs
            .iter()
            .map(|kv| format!("{}={}", kv.key, kv.value))
            .collect(),
        hostname: Some(pending.pcfg.hostname.clone()).filter(|h| !h.is_empty()),
        cwd: "/".into(),
        uid: 0,
        gid: 0,
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
    use runtime::cri_json::{ContainerMetadata, ImageSpec, SandboxMetadata};
    use runtime::{ContainerConfig, PodSandboxConfig};

    fn pending(command: Vec<String>) -> Pending {
        // Only pair args with a real command: the commandless rejection
        // case must have neither.
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

    #[test]
    fn maps_the_cri_pair_onto_the_engine_spec() {
        let p = pending(vec!["/bin/echo".into()]);
        let spec = container_spec("c-1", Path::new("/tmp/rootfs"), &p, "/proc/9/ns/net".into())
            .expect("spec");
        assert_eq!(spec.argv, vec!["/bin/echo", "-v"]);
        assert_eq!(spec.netns.as_deref(), Some("/proc/9/ns/net"));
        assert_eq!(spec.log_path.as_deref(), Some("/tmp/logs/pod/web.0.log"));
        assert_eq!(spec.hostname.as_deref(), Some("p"));
        assert!(!spec.new_net && !spec.pause);
    }

    #[test]
    fn rejects_commandless_containers() {
        assert!(container_spec("c-1", Path::new("/r"), &pending(vec![]), "n".to_string()).is_err());
    }
}
