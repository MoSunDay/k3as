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
