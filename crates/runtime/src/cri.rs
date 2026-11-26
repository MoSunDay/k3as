//! CRI driver: vendored-crictl subprocess (TODO **T4.2**, decision **Q26**
//! route B).
//!
//! Every CRI call is one short-lived `crictl --runtime-endpoint unix://<sock>`
//! subprocess (~20ms, spawn-dominated — measured in the Q26 spike). argv
//! construction is pure ([`CriCtl::argv`]) and unit-tested without spawning;
//! sandbox/container configs cross the process boundary as owner-only (0600,
//! `O_EXCL`) temp JSON files with unique names, removed even on error.
//! Route A (native gRPC) only lands if T4.2 ever needs streaming/watch.
//!
//! Every invocation is bounded by `bounded_output` (item **C13**): the
//! child is `kill_on_drop`, so a timed-out crictl is SIGKILLed and reaped
//! rather than left running as an orphan.

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

/// Wall-clock nonce (`-<nanos>`) for the C2 retry name, unpredictable when planted.
fn nonce() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!("-{nanos}")
}

/// `<tmp>/init-pro-cri-<tag>-<pid>-<counter>[-<nanos>].json`; `retry` is
/// the C2 second name after a `create_new` collision.
fn temp_config_path(tag: &str, retry: bool) -> PathBuf {
    let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let suffix = if retry { nonce() } else { String::new() };
    std::env::temp_dir().join(format!(
        "init-pro-cri-{tag}-{}-{n}{suffix}.json",
        std::process::id()
    ))
}

fn path_str(p: &Path) -> String {
    p.display().to_string()
}

async fn remove_quiet(path: &Path) {
    let _ = tokio::fs::remove_file(path).await;
}

/// `Command::output` under a wall-clock budget (item **C13**).
///
/// `kill_on_drop(true)` is the load-bearing half: when the budget expires,
/// `tokio::time::timeout` drops the `output()` future, which drops the
/// `Child`; the dropped handle then (a) sends SIGKILL from tokio's `Drop`
/// impl and (b) hands the corpse to tokio's orphan-queue reaper task, which
/// `waitpid()`s it. So a slow `crictl pull` (30 s default, routinely
/// exceeded under registry pressure) can no longer outlive its caller as an
/// orphan holding pipes, a CRI lock and CPU.
async fn bounded_output(
    cmd: &mut Command,
    budget: Duration,
    what: &str,
) -> io::Result<std::process::Output> {
    cmd.kill_on_drop(true);
    tokio::time::timeout(budget, cmd.output())
        .await
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                format!("{what} timed out after {budget:?}"),
            )
        })?
}

/// Error mapping shared by every crictl invocation: a budget expiry stays
/// `CriError::Io(TimedOut)` (the caller may retry); anything else, notably
/// an exec failure, stays `CriError::Spawn` (item C13 keeps the historical
/// split so callers match on the same variants as before).
fn output_error(err: io::Error) -> CriError {
    if err.kind() == io::ErrorKind::TimedOut {
        CriError::Io(err)
    } else {
        CriError::Spawn(err)
    }
}

/// Create `path` (`O_CREAT|O_EXCL`, mode 0600) + write `json` (C2); `Ok(false)` = name taken.
async fn create_private(path: &Path, json: &[u8]) -> io::Result<bool> {
    use tokio::io::AsyncWriteExt as _;
    let mut options = tokio::fs::OpenOptions::new();
    options.create_new(true).write(true).mode(0o600);
    let mut file = match options.open(path).await {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => return Ok(false),
        Err(e) => return Err(e),
    };
    file.write_all(json).await?;
    file.flush().await?;
    Ok(true)
}

/// Serialize `value` to a fresh temp file; the caller must remove it.
/// Batch D (C2): it carries Secret envs -- created `O_EXCL` + 0600, and a
/// taken name is skipped for the nonce-suffixed retry name, never reused.
async fn write_temp_config<T: serde::Serialize>(tag: &str, value: &T) -> Result<PathBuf, CriError> {
    let json = serde_json::to_vec(value)
        .map_err(|e| CriError::Parse(format!("config serialize failed: {e}")))?;
    let mut path = temp_config_path(tag, false);
    for _ in 0..2 {
        if create_private(&path, &json).await.map_err(CriError::Io)? {
            return Ok(path);
        }
        path = temp_config_path(tag, true);
    }
    let taken = io::Error::new(io::ErrorKind::AlreadyExists, "name taken twice");
    Err(CriError::Io(taken))
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

    /// One crictl invocation: piped stdout/stderr, timeout-guarded (C13: the
    /// child is `kill_on_drop`, so a timed-out call never leaks a crictl),
    /// stdout returned as a String on exit 0.
    async fn run(&self, args: &[&str]) -> Result<String, CriError> {
        let mut cmd = Command::new(&self.bin);
        cmd.args(args).stdout(Stdio::piped()).stderr(Stdio::piped());
        let output = bounded_output(&mut cmd, self.timeout, "crictl")
            .await
            .map_err(output_error)?;
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

    /// One-shot `crictl exec --timeout <SECONDS> <id> <cmd...>` (TODO **T4.2
    /// Scope C**, decision **Q31** — the primitive behind the kubelet's
    /// `POST /pods/{uid}/containers/{c}/exec` route).
    ///
    /// Unlike [`CriCtl::run`] a NONZERO container exit code is a normal
    /// outcome: it is returned as `(rc, merged_output)`. Only spawn/io/
    /// timeout/serialize failures are [`Err`].
    ///
    /// Merge choice (v1 simplification): stdout and stderr are captured in
    /// two separate pipes and concatenated stdout-then-stderr into one
    /// lossy-UTF-8 buffer. This keeps the merge a pure byte concat the unit
    /// tests can reason about, at the cost of no real-time interleaving.
    ///
    /// The crictl-side `--timeout` flag and the tokio-level deadline are set
    /// to the same duration (the explicit-flag lesson from the T4.2 flake:
    /// never rely on only one layer).
    pub async fn exec_container(
        &self,
        container_id: &str,
        cmd: &[String],
        timeout: Duration,
    ) -> Result<(i32, String), CriError> {
        if cmd.is_empty() {
            return Err(CriError::Parse(
                "exec command must be non-empty (no shell, array of argv words)".to_string(),
            ));
        }
        let argv = self.exec_argv(container_id, cmd, timeout);
        let mut command = Command::new(&self.bin);
        command
            .args(&argv)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // C13: same bounded shape as `run`; an exec that overruns its
        // budget is SIGKILLed and reaped instead of orphaned.
        let output = bounded_output(&mut command, timeout, "crictl exec")
            .await
            .map_err(output_error)?;
        // Merged output: stdout first, then stderr (documented above).
        let mut merged = output.stdout;
        merged.extend_from_slice(&output.stderr);
        Ok((
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&merged).into_owned(),
        ))
    }

    /// Pure argv for [`CriCtl::exec_container`] (crictl's `--timeout` takes
    /// RAW SECONDS — no unit suffix, unlike stop: `crictl exec --timeout N
    /// <id> <words...>`), following the same pattern as [`CriCtl::argv`] so
    /// it stays unit-tested without spawning. Owned `String`s because the
    /// exec words cross from caller-owned vectors.
    fn exec_argv(&self, container_id: &str, cmd: &[String], timeout: Duration) -> Vec<String> {
        let mut argv = vec![
            "--runtime-endpoint".to_string(),
            self.endpoint.clone(),
            "exec".to_string(),
            "--timeout".to_string(),
            timeout.as_secs().max(1).to_string(),
            container_id.to_string(),
        ];
        argv.extend(cmd.iter().cloned());
        argv
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;

    fn cri() -> CriCtl {
        CriCtl::new(
            PathBuf::from("/x/crictl"),
            "unix:///dd/run/containerd/containerd.sock".into(),
        )
    }

    fn name_shape_ok(name: &str) -> bool {
        name.starts_with("init-pro-cri-runp-")
            && name.contains(&format!("-{}-", std::process::id()))
            && name.ends_with(".json")
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
    fn argv_exec_orders_timeout_flag_before_id_and_words() {
        // T4.2 Scope C: `crictl exec --timeout 30 <id> <words...>`; the
        // explicit crictl-side --timeout is part of the contract (Q31).
        let c = cri();
        let argv = c.exec_argv(
            "cid0",
            &["/bin/wfstep".to_string(), "hello".to_string()],
            Duration::from_secs(30),
        );
        assert_eq!(
            argv,
            vec![
                "--runtime-endpoint",
                "unix:///dd/run/containerd/containerd.sock",
                "exec",
                "--timeout",
                "30",
                "cid0",
                "/bin/wfstep",
                "hello"
            ]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>()
        );
    }

    #[test]
    fn argv_exec_clamps_subsecond_timeouts_to_one_second() {
        // A zero/sub-second caller timeout must still yield a well-formed
        // (>= 1s) crictl flag: the tokio deadline races it anyway.
        let c = cri();
        assert_eq!(
            c.exec_argv("cid", &[], Duration::from_millis(40))
                .as_slice()[3..5],
            ["--timeout", "1"]
        );
    }

    #[test]
    fn temp_config_paths_are_unique_and_well_formed() {
        let a = temp_config_path("runp", false);
        let b = temp_config_path("runp", false);
        assert_ne!(a, b, "atomic counter prevents collisions");
        let name = a.file_name().unwrap().to_string_lossy().to_string();
        assert!(name_shape_ok(&name), "{name}");
        let retry = temp_config_path("runp", true);
        let rname = retry.file_name().unwrap().to_string_lossy().to_string();
        assert!(name_shape_ok(&rname) && retry != temp_config_path("runp", true));
    }

    /// Batch D (C2): owner-only + intact; a taken name is never written through.
    #[tokio::test]
    async fn temp_config_is_owner_only_and_skips_a_taken_name() {
        let taken = std::env::temp_dir();
        assert!(!create_private(&taken, b"payload").await.unwrap() && taken.is_dir());
        let path = write_temp_config("c2", &serde_json::json!({ "envs": [{ "value": "v" }] }))
            .await
            .unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "Secret envs stay owner-only");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains(r#""value":"v""#), "content intact: {text}");
        let _ = std::fs::remove_file(&path);
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

    // ---- item C13: bounded crictl children are killed + reaped ----

    /// `CriError` mapping preserves the historical variant split.
    #[test]
    fn output_error_keeps_timeout_and_spawn_apart() {
        let timed_out = output_error(io::Error::new(io::ErrorKind::TimedOut, "slow"));
        assert!(
            matches!(timed_out, CriError::Io(ref e) if e.kind() == io::ErrorKind::TimedOut),
            "a budget expiry must stay CriError::Io(TimedOut)"
        );
        let spawn = output_error(io::Error::new(io::ErrorKind::NotFound, "no crictl"));
        assert!(
            matches!(spawn, CriError::Spawn(ref e) if e.kind() == io::ErrorKind::NotFound),
            "an exec failure must stay CriError::Spawn"
        );
    }

    /// Pid recorded by a probe child (`echo $$ > <file>; exec sleep 30`).
    fn probe_pid(pid_file: &Path) -> Option<u32> {
        std::fs::read_to_string(pid_file).ok()?.trim().parse().ok()
    }

    /// `/proc/<pid>/stat` state letter (`R`/`S` = alive, `Z` = dead but
    /// unreaped); `None` once the process is gone (reaped).
    fn proc_state(pid: u32) -> Option<char> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        // `pid (comm) S ...`: `comm` may contain spaces and parens, so the
        // state is the first non-space byte after the LAST ')'.
        let after = stat.rsplit_once(')')?.1;
        after.trim().chars().next()
    }

    /// Poll until the pid is neither running nor sleeping (SIGKILL landed).
    async fn wait_dead(pid: u32, budget: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + budget;
        loop {
            match proc_state(pid) {
                None | Some('Z') | Some('X') => return true,
                Some(_) if tokio::time::Instant::now() >= deadline => return false,
                Some(_) => tokio::time::sleep(Duration::from_millis(25)).await,
            }
        }
    }

    /// Poll until `/proc/<pid>` disappears: tokio's orphan-queue reaper has
    /// `waitpid()`ed the corpse (no zombie left behind).
    async fn wait_reaped(pid: u32, budget: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + budget;
        while Path::new(&format!("/proc/{pid}")).exists() {
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        true
    }

    /// Write an executable fake crictl that publishes its pid then sleeps.
    /// Created with mode 0o755 in one step: a separate `chmod` makes overlayfs
    /// copy up the metadata, and an exec racing that copy-up answers ETXTBSY.
    fn fake_crictl(dir: &Path, pid_file: &Path) -> PathBuf {
        use std::os::unix::fs::OpenOptionsExt;
        let bin = dir.join("crictl");
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o755)
            .open(&bin)
            .unwrap();
        f.write_all(
            format!(
                "#!/bin/sh\necho $$ > '{}'\nexec sleep 30\n",
                pid_file.display()
            )
            .as_bytes(),
        )
        .unwrap();
        // Flush the bytes before the exec: overlayfs answers ETXTBSY to an
        // exec that lands inside the copy-up of a freshly written file.
        f.sync_all().unwrap();
        bin
    }

    /// Run one CRI call, tolerating overlayfs's ETXTBSY on a just-written
    /// fake crictl (a metadata copy-up can still race the exec under a
    /// parallel test run). Retrying is safe: a failed exec spawned nothing.
    async fn retry_txtbusy<T, F, Fut>(mut call: F) -> Result<T, CriError>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Result<T, CriError>>,
    {
        let mut attempt = 0;
        loop {
            match call().await {
                Err(CriError::Spawn(ref e)) if e.kind() == io::ErrorKind::ExecutableFileBusy => {
                    attempt += 1;
                    assert!(attempt < 40, "fake crictl stays ETXTBSY");
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                other => return other,
            }
        }
    }

    fn c13_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cri-c13-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn bounded_output_returns_a_fast_child_output() {
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg("echo crictl-ok")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let out = bounded_output(&mut cmd, Duration::from_secs(5), "probe")
            .await
            .expect("a fast child finishes inside its budget");
        assert!(out.status.success());
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "crictl-ok");
    }

    #[tokio::test]
    async fn bounded_output_kills_and_reaps_a_child_past_its_budget() {
        // C13: before `kill_on_drop` the dropped future left this child
        // running for the full 30s, holding pipes and CPU.
        let dir = c13_dir("bounded");
        let pid_file = dir.join("pid");
        let script = format!("echo $$ > '{}'; exec sleep 30", pid_file.display());
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg(&script)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let err = bounded_output(&mut cmd, Duration::from_millis(300), "probe")
            .await
            .expect_err("a 30s child must exceed a 300ms budget");
        assert_eq!(err.kind(), io::ErrorKind::TimedOut, "{err}");
        let pid = probe_pid(&pid_file).expect("the probe recorded its pid");
        assert!(
            wait_dead(pid, Duration::from_secs(2)).await,
            "the over-budget child must be SIGKILLed (C13)"
        );
        assert!(
            wait_reaped(pid, Duration::from_secs(5)).await,
            "tokio's reaper must waitpid() the killed child (C13)"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn run_times_out_without_orphaning_crictl() {
        // The `pull` path: a 30s crictl against a 300ms budget.
        let dir = c13_dir("run");
        let pid_file = dir.join("pid");
        let c = CriCtl::new(
            fake_crictl(&dir, &pid_file),
            "unix:///nowhere.sock".to_string(),
        )
        .with_timeout(Duration::from_millis(300));
        let err = retry_txtbusy(|| c.pull_image("init-pro.local/pause:0.1"))
            .await
            .expect_err("the fake crictl overruns its budget");
        match err {
            CriError::Io(ref e) => {
                assert_eq!(e.kind(), io::ErrorKind::TimedOut, "budget expiry: {e}")
            }
            ref other => panic!("expected CriError::Io(TimedOut), got {other:?}"),
        }
        let pid = probe_pid(&pid_file).expect("the fake crictl recorded its pid");
        assert!(
            wait_dead(pid, Duration::from_secs(2)).await,
            "crictl killed (C13)"
        );
        assert!(
            wait_reaped(pid, Duration::from_secs(5)).await,
            "crictl reaped (C13)"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn exec_container_times_out_without_orphaning_crictl() {
        // The `exec` path (T4.2 Scope C, Q31) races the same budget.
        let dir = c13_dir("exec");
        let pid_file = dir.join("pid");
        let c = CriCtl::new(
            fake_crictl(&dir, &pid_file),
            "unix:///nowhere.sock".to_string(),
        )
        .with_timeout(Duration::from_secs(30));
        let words = vec!["/bin/true".to_string()];
        let err = retry_txtbusy(|| c.exec_container("cid0", &words, Duration::from_millis(300)))
            .await
            .expect_err("the fake crictl overruns the exec budget");
        match err {
            CriError::Io(ref e) => {
                assert_eq!(e.kind(), io::ErrorKind::TimedOut, "budget expiry: {e}")
            }
            ref other => panic!("expected CriError::Io(TimedOut), got {other:?}"),
        }
        let pid = probe_pid(&pid_file).expect("the fake crictl recorded its pid");
        assert!(
            wait_dead(pid, Duration::from_secs(2)).await,
            "crictl killed (C13)"
        );
        assert!(
            wait_reaped(pid, Duration::from_secs(5)).await,
            "crictl reaped (C13)"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
