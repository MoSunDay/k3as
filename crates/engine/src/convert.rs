//! Protocol spec -> `nsrun::RunSpec` (no unsafe, no I/O).

use std::path::PathBuf;

use engine_proto::ContainerSpec;
use nsrun::{BindMount, CgroupLimits, RunSpec};

pub fn to_run_spec(spec: &ContainerSpec) -> Result<RunSpec, String> {
    if spec.id.is_empty() {
        return Err("container id is empty".to_string());
    }
    if spec.rootfs.is_empty() {
        return Err("rootfs is empty".to_string());
    }
    let mut run = RunSpec::new(PathBuf::from(&spec.rootfs), spec.argv.clone());
    run.env = spec.env.clone();
    run.hostname = spec.hostname.clone();
    run.cwd = PathBuf::from(if spec.cwd.is_empty() { "/" } else { &spec.cwd });
    run.uid = spec.uid;
    run.gid = spec.gid;
    run.new_net = spec.new_net;
    run.netns = spec.netns.as_deref().map(std::path::PathBuf::from);
    // T4.15 (Q60): the host-root mode is a dedicated variant, not a
    // rootfs spelling — contradictory network/overlay requests fail with
    // an actionable host-direct error instead of being silently dropped
    // (HD5), and the rootfs normalizes to the literal `/`.
    run.host_root = spec.host_root;
    if spec.host_root {
        if spec.new_net || spec.netns.is_some() {
            return Err(
                "host-direct: new_net/netns are incompatible with the host root \
                 (Q60 keeps the host network; no netns is created or joined)"
                    .to_string(),
            );
        }
        if spec.overlay.is_some() {
            return Err("host-direct: overlay is incompatible with the host root \
                 (Q60 mounts no image rootfs)"
                .to_string());
        }
        run.rootfs = PathBuf::from("/");
        run.netns = None;
    }
    // T4.13 M3 (Q53): per-container overlayfs view passthrough.
    run.overlay = spec.overlay.as_ref().map(|ov| nsrun::Overlay {
        lower: ov.lower.iter().map(PathBuf::from).collect(),
        upper: PathBuf::from(&ov.upper),
        work: PathBuf::from(&ov.work),
    });
    run.pause = spec.pause;
    run.no_new_privs = spec.no_new_privs;
    run.mounts = spec
        .mounts
        .iter()
        .map(|m| {
            let mut bind = BindMount::new(m.source.clone(), m.target.clone());
            bind.readonly = m.readonly;
            bind
        })
        .collect();
    let limits = CgroupLimits {
        memory_max: spec.memory_max,
        memory_swap_max: spec.memory_max.map(|_| 0),
        cpu_max: spec.cpu_max.clone(),
        pids_max: spec.pids_max,
    };
    // A named cgroup without limits would be dropped on the floor by
    // `nsrun::spawn` (it creates the cgroup only when there are limits), so
    // a client that asks for one gets an error instead of the default group.
    if spec.cgroup_name.is_some() && limits.is_empty() {
        return Err(
            "cgroup_name needs limits (memory_max/cpu_max/pids_max); without them it is a no-op"
                .to_string(),
        );
    }
    if !limits.is_empty() {
        run.cgroup = limits;
        run.cgroup_name = Some(
            spec.cgroup_name
                .clone()
                .unwrap_or_else(|| sanitize(&spec.id)),
        );
    }
    run.validate().map_err(|e| e.to_string())?;
    Ok(run)
}

fn sanitize(id: &str) -> String {
    let mut out: String = id
        .chars()
        .take(48)
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '-'
            }
        })
        .collect();
    if out.is_empty() {
        out.push_str("container");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_mounts_env_and_limits() {
        let mut spec = ContainerSpec::new("c1", "/srv/rootfs", vec!["/bin/true".into()]);
        spec.env = vec!["A=1".into()];
        spec.mounts.push(engine_proto::MountSpec {
            source: "/etc/hosts".into(),
            target: "/etc/hosts".into(),
            readonly: true,
        });
        spec.memory_max = Some(1024);
        let run = to_run_spec(&spec).unwrap();
        assert_eq!(run.env, ["A=1"]);
        assert_eq!(run.mounts.len(), 1);
        assert!(run.mounts[0].readonly);
        assert_eq!(run.cgroup.memory_max, Some(1024));
        assert_eq!(run.cgroup.memory_swap_max, Some(0));
        assert_eq!(run.cgroup_name.as_deref(), Some("c1"));
    }

    #[test]
    fn maps_netns_and_pause() {
        let mut spec = ContainerSpec::new("c1", "/r", vec!["/bin/true".into()]);
        spec.netns = Some("/proc/42/ns/net".into());
        spec.pause = true;
        spec.log_path = Some("/var/log/c.log".into());
        let run = to_run_spec(&spec).unwrap();
        assert_eq!(
            run.netns.as_deref(),
            Some(std::path::Path::new("/proc/42/ns/net"))
        );
        assert!(run.pause);
    }

    #[test]
    fn maps_overlay_passthrough() {
        let mut spec = ContainerSpec::new("c1", "/views/c/mnt", vec!["/bin/true".into()]);
        spec.overlay = Some(engine_proto::OverlaySpec {
            lower: vec!["/layers/l1".into(), "/layers/l2".into()],
            upper: "/views/c/upper".into(),
            work: "/views/c/work".into(),
        });
        let run = to_run_spec(&spec).unwrap();
        let ov = run.overlay.as_ref().expect("overlay mapped");
        assert_eq!(
            ov.lower,
            [PathBuf::from("/layers/l1"), PathBuf::from("/layers/l2")]
        );
        assert_eq!(ov.upper, PathBuf::from("/views/c/upper"));
        assert_eq!(ov.work, PathBuf::from("/views/c/work"));
        // Defaults stay plain-dir rootfs.
        assert!(
            to_run_spec(&ContainerSpec::new("c2", "/r", vec!["/bin/true".into()]))
                .unwrap()
                .overlay
                .is_none()
        );
    }

    #[test]
    fn maps_host_root_as_a_dedicated_variant() {
        // Q60/HD2: rootfs "/" + host_root carries no image meaning; the
        // child keeps the host root, joins no netns, mounts no overlay.
        let spec = ContainerSpec::new("c1", "/", vec!["/usr/bin/true".into()]);
        let mut spec = spec;
        spec.host_root = true;
        let run = to_run_spec(&spec).unwrap();
        assert!(run.host_root);
        assert_eq!(run.rootfs, PathBuf::from("/"));
        assert!(run.netns.is_none());
        assert!(!run.new_net);
        assert!(run.overlay.is_none());
        // Contradictions fail with actionable host-direct text (HD5).
        let mut net = ContainerSpec::new("c1", "/", vec!["/bin/true".into()]);
        net.host_root = true;
        net.netns = Some("/proc/7/ns/net".into());
        assert!(to_run_spec(&net).unwrap_err().contains("host-direct"));
        let mut ov = ContainerSpec::new("c1", "/", vec!["/bin/true".into()]);
        ov.host_root = true;
        ov.overlay = Some(engine_proto::OverlaySpec {
            lower: vec!["/layers/l1".into()],
            upper: "/views/c/upper".into(),
            work: "/views/c/work".into(),
        });
        assert!(to_run_spec(&ov).unwrap_err().contains("host-direct"));
    }

    #[test]
    fn rejects_empty_id() {
        let spec = ContainerSpec::new("", "/r", vec!["/bin/true".into()]);
        assert!(to_run_spec(&spec).is_err());
    }

    #[test]
    fn rejects_a_named_cgroup_without_limits() {
        let mut spec = ContainerSpec::new("c1", "/r", vec!["/bin/true".into()]);
        spec.cgroup_name = Some("demo".into());
        let err = to_run_spec(&spec).expect_err("cgroup_name needs limits");
        assert!(err.contains("no-op"), "{err}");
        // With a limit the same name is carried through verbatim.
        spec.pids_max = Some(8);
        let run = to_run_spec(&spec).unwrap();
        assert_eq!(run.cgroup_name.as_deref(), Some("demo"));
        assert_eq!(run.cgroup.pids_max, Some(8));
    }

    #[test]
    fn sanitizes_cgroup_names() {
        assert_eq!(sanitize("pod/../../x"), "pod-..-..-x");
    }
}
