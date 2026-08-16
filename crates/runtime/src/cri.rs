//! CRI driver: vendored-crictl subprocess (TODO **T4.2**, decision **Q26**
//! route B).
//!
//! Every CRI call is one short-lived `crictl --runtime-endpoint unix://<sock>`
//! subprocess (~20ms, spawn-dominated — measured in the Q26 spike). argv
//! construction is pure ([`CriCtl::argv`]) and unit-tested without spawning;
//! sandbox/container configs cross the process boundary as temp JSON files
//! with unique, pid+counter-suffixed names that are removed even on error.
//! Route A (native gRPC) only lands if T4.2 ever needs streaming/watch.

use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use thiserror::Error;
use tokio::process::Command;

use crate::cri_json::{
    parse_containers, parse_images, parse_inspect_pod_sandbox, parse_sandboxes, ContainerConfig,
    CriContainer, CriImage, CriSandbox, PodSandboxConfig, PodSandboxInspect,
};
use crate::AgentRuntimePaths;

/// Default per-call timeout (crictl calls are fast; pulls are retried by
/// callers, not held open longer).
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Monotonic suffix so concurrent temp config files never collide.
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Failure modes of one crictl invocation.
#[derive(Debug, Error)]
pub enum CriError {
    #[error("failed to spawn crictl: {0}")]
    Spawn(#[source] io::Error),
    #[error("crictl exited with code {code}: {stderr}")]
    Exit { code: i32, stderr: String },
    #[error("crictl output: {0}")]
    Parse(String),
    #[error("crictl I/O: {0}")]
    Io(#[source] io::Error),
}

/// Handle to the staged crictl binary + the agent's CRI socket.
#[derive(Debug, Clone)]
pub struct CriCtl {
    bin: PathBuf,
    endpoint: String,
    timeout: Duration,
}

/// The staged crictl binary, when the vendor bundle was staged (T4.1).
pub fn staged_crictl(paths: &AgentRuntimePaths) -> Option<PathBuf> {
    let bin = paths.runtime_dir.join("crictl");
    bin.is_file().then_some(bin)
}

/// Pure subcommand argv builder: `<sub> <extra...>` (no global flags).
fn cmd_args<'a>(sub: &'a str, extra: &'a [&'a str]) -> Vec<&'a str> {
    let mut args = Vec::with_capacity(1 + extra.len());
    args.push(sub);
    args.extend_from_slice(extra);
    args
}

/// Unique temp path for one crictl config file:
/// `<tmp>/init-pro-cri-<tag>-<pid>-<counter>.json`.
fn temp_config_path(tag: &str) -> PathBuf {
    let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "init-pro-cri-{tag}-{}-{n}.json",
        std::process::id()
    ))
}

fn path_str(p: &Path) -> String {
    p.display().to_string()
}

async fn remove_quiet(path: &Path) {
    let _ = tokio::fs::remove_file(path).await;
}

/// Serialize `value` to a fresh unique temp file; the caller must remove it.
async fn write_temp_config<T: serde::Serialize>(tag: &str, value: &T) -> Result<PathBuf, CriError> {
    let json = serde_json::to_vec(value)
        .map_err(|e| CriError::Parse(format!("config serialize failed: {e}")))?;
    let path = temp_config_path(tag);
    tokio::fs::write(&path, &json).await.map_err(CriError::Io)?;
    Ok(path)
}

impl CriCtl {
    pub fn new(bin: PathBuf, endpoint: String) -> Self {
        Self {
            bin,
            endpoint,
            timeout: DEFAULT_TIMEOUT,
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Driver for a staged agent runtime: staged crictl (T4.1) + the
    /// supervisor's socket. `None` when the bundle has no crictl yet.
    pub fn for_paths(paths: &AgentRuntimePaths) -> Option<Self> {
        let bin = staged_crictl(paths)?;
        Some(Self::new(bin, format!("unix://{}", paths.socket.display())))
    }

    /// Global flags preceding every subcommand.
    fn base_args(&self) -> Vec<&str> {
        vec!["--runtime-endpoint", &self.endpoint]
    }

    /// Full argv after the binary: global flags + subcommand + extras.
    fn argv<'a>(&'a self, sub: &'a str, extra: &'a [&'a str]) -> Vec<&'a str> {
        let mut args = self.base_args();
        args.extend(cmd_args(sub, extra));
        args
    }

    /// One crictl invocation: piped stdout/stderr, timeout-guarded, stdout
    /// returned as a String on exit 0.
    async fn run(&self, args: &[&str]) -> Result<String, CriError> {
        let mut cmd = Command::new(&self.bin);
        cmd.args(args).stdout(Stdio::piped()).stderr(Stdio::piped());
        let output = tokio::time::timeout(self.timeout, cmd.output())
            .await
            .map_err(|_| {
                CriError::Io(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("crictl timed out after {:?}", self.timeout),
                ))
            })?
            .map_err(CriError::Spawn)?;
        if !output.status.success() {
            return Err(CriError::Exit {
                code: output.status.code().unwrap_or(-1),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            });
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    /// `crictl version` — RuntimeService.Version round-trip (Q26 evidence).
    pub async fn version(&self) -> Result<String, CriError> {
        self.run(&self.argv("version", &[]))
            .await
            .map(|s| s.trim().to_string())
    }

    /// `crictl pull <image>` (registry egress; callers bound retries).
    pub async fn pull_image(&self, image: &str) -> Result<(), CriError> {
        self.run(&self.argv("pull", &[image])).await.map(|_| ())
    }

    /// `crictl images -o json`.
    pub async fn list_images(&self) -> Result<Vec<CriImage>, CriError> {
        let out = self.run(&self.argv("images", &["-o", "json"])).await?;
        parse_images(&out).map_err(CriError::Parse)
    }

    /// `crictl runp <pod-config.json>` -> the new sandbox id.
    pub async fn run_pod_sandbox(&self, cfg: &PodSandboxConfig) -> Result<String, CriError> {
        let file = write_temp_config("runp", cfg).await?;
        let f = path_str(&file);
        let res = self.run(&self.argv("runp", &[&f])).await;
        remove_quiet(&file).await;
        res.map(|s| s.trim().to_string())
    }

    /// `crictl create <podId> <container.json> <pod.json>` -> container id.
    pub async fn create_container(
        &self,
        pod_sandbox_id: &str,
        ccfg: &ContainerConfig,
        pcfg: &PodSandboxConfig,
    ) -> Result<String, CriError> {
        let cfile = write_temp_config("create-container", ccfg).await?;
        let pfile = match write_temp_config("create-pod", pcfg).await {
            Ok(p) => p,
            Err(e) => {
                remove_quiet(&cfile).await;
                return Err(e);
            }
        };
        let (cs, ps) = (path_str(&cfile), path_str(&pfile));
        let res = self
            .run(&self.argv("create", &[pod_sandbox_id, &cs, &ps]))
            .await;
        remove_quiet(&cfile).await;
        remove_quiet(&pfile).await;
        res.map(|s| s.trim().to_string())
    }

    /// `crictl start <id>`.
    pub async fn start_container(&self, id: &str) -> Result<(), CriError> {
        self.run(&self.argv("start", &[id])).await.map(|_| ())
    }

    /// `crictl stop --timeout <n> <id>`.
    pub async fn stop_container(&self, id: &str, timeout_secs: u32) -> Result<(), CriError> {
        let t = timeout_secs.to_string();
        self.run(&self.argv("stop", &["--timeout", &t, id]))
            .await
            .map(|_| ())
    }

    /// `crictl rm <id>`.
    pub async fn remove_container(&self, id: &str) -> Result<(), CriError> {
        self.run(&self.argv("rm", &[id])).await.map(|_| ())
    }

    /// `crictl stopp <id>` (stops all containers in the sandbox, then it).
    pub async fn stop_pod_sandbox(&self, id: &str) -> Result<(), CriError> {
        self.run(&self.argv("stopp", &[id])).await.map(|_| ())
    }

    /// `crictl rmp <id>`.
    pub async fn remove_pod_sandbox(&self, id: &str) -> Result<(), CriError> {
        self.run(&self.argv("rmp", &[id])).await.map(|_| ())
    }

    /// `crictl ps -a -o json` (all states).
    pub async fn list_containers(&self) -> Result<Vec<CriContainer>, CriError> {
        let out = self.run(&self.argv("ps", &["-a", "-o", "json"])).await?;
        parse_containers(&out).map_err(CriError::Parse)
    }

    /// `crictl pods -o json`.
    pub async fn list_pod_sandboxes(&self) -> Result<Vec<CriSandbox>, CriError> {
        let out = self.run(&self.argv("pods", &["-o", "json"])).await?;
        parse_sandboxes(&out).map_err(CriError::Parse)
    }

    /// `crictl inspectp <id> -o json` — full sandbox status incl. CNI IP
    /// (Sprint 18 / S1; `status.network.ip`).
    pub async fn inspect_pod_sandbox(&self, id: &str) -> Result<PodSandboxInspect, CriError> {
        let out = self
            .run(&self.argv("inspectp", &["-o", "json", id]))
            .await?;
        parse_inspect_pod_sandbox(&out).map_err(CriError::Parse)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cri() -> CriCtl {
        CriCtl::new(
            PathBuf::from("/x/crictl"),
            "unix:///dd/run/containerd/containerd.sock".into(),
        )
    }

    #[test]
    fn argv_ps_lists_all_states_as_json() {
        assert_eq!(
            cri().argv("ps", &["-a", "-o", "json"]),
            vec![
                "--runtime-endpoint",
                "unix:///dd/run/containerd/containerd.sock",
                "ps",
                "-a",
                "-o",
                "json"
            ]
        );
    }

    #[test]
    fn argv_pull_runp_pods_version() {
        let c = cri();
        assert_eq!(
            c.argv("pull", &["init-pro.local/pause:0.1"])[2..],
            ["pull", "init-pro.local/pause:0.1"]
        );
        assert_eq!(
            c.argv("runp", &["/tmp/p.json"])[2..],
            ["runp", "/tmp/p.json"]
        );
        assert_eq!(c.argv("pods", &["-o", "json"])[2..], ["pods", "-o", "json"]);
        assert_eq!(c.argv("version", &[])[2..], ["version"]);
    }

    #[test]
    fn argv_create_orders_pod_then_configs() {
        assert_eq!(
            cmd_args("create", &["sb1", "/tmp/c.json", "/tmp/p.json"]),
            vec!["create", "sb1", "/tmp/c.json", "/tmp/p.json"]
        );
    }

    #[test]
    fn argv_stop_passes_timeout_flag() {
        assert_eq!(
            cmd_args("stop", &["--timeout", "10", "cid"]),
            vec!["stop", "--timeout", "10", "cid"]
        );
        assert_eq!(cmd_args("rm", &["cid"]), vec!["rm", "cid"]);
        assert_eq!(cmd_args("stopp", &["sb1"]), vec!["stopp", "sb1"]);
        assert_eq!(cmd_args("rmp", &["sb1"]), vec!["rmp", "sb1"]);
    }

    #[test]
    fn temp_config_paths_are_unique_and_well_formed() {
        let a = temp_config_path("runp");
        let b = temp_config_path("runp");
        assert_ne!(a, b, "atomic counter prevents collisions");
        let name = a.file_name().unwrap().to_string_lossy().to_string();
        assert!(name.starts_with("init-pro-cri-runp-"), "{name}");
        assert!(
            name.contains(&format!("-{}-", std::process::id())),
            "{name}"
        );
        assert!(name.ends_with(".json"), "{name}");
    }

    #[test]
    fn for_paths_requires_a_staged_crictl() {
        let dd = std::env::temp_dir().join(format!("cri-none-{}", std::process::id()));
        let paths = AgentRuntimePaths::for_data_dir(&dd);
        assert!(staged_crictl(&paths).is_none());
        assert!(CriCtl::for_paths(&paths).is_none());

        std::fs::create_dir_all(&paths.runtime_dir).unwrap();
        std::fs::write(paths.runtime_dir.join("crictl"), b"#!/bin/sh\n").unwrap();
        let bin = staged_crictl(&paths).expect("staged crictl found");
        let c = CriCtl::for_paths(&paths).expect("driver built");
        assert_eq!(bin, paths.runtime_dir.join("crictl"));
        assert_eq!(c.timeout, DEFAULT_TIMEOUT);
        assert_eq!(
            c.argv("version", &[])[1],
            format!("unix://{}", paths.socket.display())
        );
        let c2 = c.clone().with_timeout(Duration::from_secs(3));
        assert_eq!(c2.timeout, Duration::from_secs(3));
        let _ = std::fs::remove_dir_all(&dd);
    }
}
