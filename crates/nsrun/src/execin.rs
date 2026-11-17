//! Exec into a running container (T4.12, Q50): a fork relay that
//! setns()-es into the target container's namespaces and exec's a new
//! process there, reporting the container-ns pid back to the parent.
//!
//! What `nsenter` + `chroot` do for a shell: relay A joins the target's
//! mnt/ipc/uts/net/cgroup/pid namespaces (pid LAST — setns(CLONE_NEWPID)
//! only moves later forks, hence the second fork) and chroot()s into
//! `/proc/<pid>/root`; relay B, born inside the container pid namespace,
//! reports its own pid down a pipe and exec's. Relay A stays in the
//! engine's pid namespace, so `ExecProc`'s `Drop` can always kill+reap
//! it. No `unsafe` lives here: every syscall goes through `sys`.

use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::path::PathBuf;
use std::time::Duration;

use crate::exec_relay::{open_ns, read_exec_report, relay_a, RelayIo};
use crate::handle::{self, Output};
use crate::sys;

/// Directory prefixes tried after argv[0] as-is when it is a bare name
/// (`sh` -> `/bin/sh`, ...). Same spirit as shell PATH resolution.
const EXEC_PREFIXES: [&str; 4] = ["/bin/", "/usr/bin/", "/sbin/", "/usr/sbin/"];

fn invalid(msg: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, msg)
}

/// Exec candidates for argv[0]: absolute paths stay as-is, bare names get
/// the classic ladder. Pure so the ladder is unit-testable.
pub(crate) fn candidates(argv0: &str) -> Vec<CString> {
    let build = |s: String| CString::new(s).ok();
    if argv0.starts_with('/') {
        return build(argv0.to_string()).into_iter().collect();
    }
    std::iter::once(argv0.to_string())
        .chain(EXEC_PREFIXES.iter().map(|p| format!("{p}{argv0}")))
        .filter_map(build)
        .collect()
}

/// What to exec inside a running container (Q50 `Exec` request).
#[derive(Debug, Clone)]
pub struct ExecSpec {
    /// Target container pid in the ENGINE's pid namespace; `/proc/<pid>`
    /// must resolve there.
    pub pid: libc::pid_t,
    /// argv, with argv[0] resolved INSIDE the container rootfs.
    pub argv: Vec<String>,
    /// `KEY=VALUE` entries; empty means an empty environment (the engine
    /// always passes the container env).
    pub env: Vec<String>,
    /// Working directory inside the container (absolute; `/` default).
    pub cwd: PathBuf,
}

impl ExecSpec {
    pub fn new(pid: libc::pid_t, argv: Vec<String>) -> Self {
        Self {
            pid,
            argv,
            env: Vec::new(),
            cwd: PathBuf::from("/"),
        }
    }

    /// Pure validation, mirroring `RunSpec::validate`.
    pub fn validate(&self) -> io::Result<()> {
        if self.argv.is_empty() || self.argv[0].is_empty() {
            return Err(invalid("no command to exec".into()));
        }
        if self.pid <= 0 {
            return Err(invalid("target pid must be positive".into()));
        }
        if !self.cwd.is_absolute() {
            return Err(invalid(format!(
                "cwd must be absolute: {}",
                self.cwd.display()
            )));
        }
        Ok(())
    }
}

/// The relay's pid report: 4 little-endian bytes. A partial frame waits
/// for more; extra bytes (never written) are ignored after the first four.
fn parse_pid_report(buf: &[u8]) -> Option<u32> {
    let b: [u8; 4] = buf.get(..4)?.try_into().ok()?;
    Some(u32::from_le_bytes(b))
}

/// An exec'd process owned by the engine: the relay pid (engine pid-ns
/// view), the non-blocking stdout/stderr capture pipes, the lazy pid
/// report, and the reaped exit status. Mirrors `handle::Child`.
pub struct ExecProc {
    pid: libc::pid_t,
    stdout: OwnedFd,
    stderr: OwnedFd,
    stdout_open: bool,
    stderr_open: bool,
    pid_report: Option<OwnedFd>,
    pid_buf: [u8; 4],
    pid_len: usize,
    container_pid: Option<u32>,
    status: Option<i32>,
    exit_sent: bool,
    grace: u8,
}

impl ExecProc {
    /// The relay pid as the engine sees it (what `Drop` reaps).
    pub fn relay_pid(&self) -> u32 {
        self.pid as u32
    }

    /// The pid of the exec'd process AS SEEN IN THE CONTAINER's pid
    /// namespace, once the report pipe delivered it.
    pub fn container_pid(&self) -> Option<u32> {
        self.container_pid
    }

    /// Next stdout/stderr chunk, or the one-shot relay exit. Semantics
    /// mirror `handle::Child::next_event`; the pid report is drained
    /// opportunistically on every poll.
    pub fn next_event(&mut self, timeout: Duration) -> io::Result<Option<Output>> {
        if self.exit_sent {
            return Ok(None);
        }
        if let Some(fd) = self.pid_report.as_ref().map(|f| f.as_raw_fd()) {
            self.poll_pid_report(fd)?;
        }
        let found = handle::poll_pipes(
            &self.stdout,
            &self.stderr,
            &mut self.stdout_open,
            &mut self.stderr_open,
            timeout,
        )?;
        let status = self.poll_status()?;
        if found.is_some() {
            self.grace = 0;
            return Ok(found);
        }
        if !self.stdout_open && !self.stderr_open {
            if let Some(code) = status {
                return Ok(Some(self.emit_exit(code)));
            }
        }
        // A grandchild may still hold the pipes: bound the drain the same
        // way `handle::Child` does (data always wins while in grace).
        if let Some(code) = status {
            if self.grace >= handle::EXIT_GRACE_SLICES {
                return Ok(Some(self.emit_exit(code)));
            }
            self.grace = self.grace.saturating_add(1);
        }
        Ok(None)
    }

    /// Bounded wait that drains stdout/stderr as `Exited` is approached.
    pub fn wait_timeout(&mut self, budget: Duration) -> io::Result<i32> {
        if self.exit_sent {
            return Err(io::Error::other("the exec exit was already reported"));
        }
        handle::wait_loop(budget, "the exec'd process did not exit", |t| {
            self.next_event(t)
        })
    }

    /// Blocking wait (mirrors `Child::wait`'s 1 h budget).
    pub fn wait(&mut self) -> io::Result<i32> {
        self.wait_timeout(Duration::from_secs(3600))
    }

    /// Deliver a signal to the relay (guarded like `Child::signal`: never
    /// signal a reaped pid). Killing the relay is how the engine stops an
    /// exec.
    pub fn signal(&mut self, signo: i32) {
        if handle::should_kill(self.status, self.exit_sent) {
            sys::signal_pid(self.pid, signo);
        }
    }

    /// Non-blocking drain of the 4-byte pid report; closes it once the
    /// frame is complete or the writer is gone (post-exec CLOEXEC).
    fn poll_pid_report(&mut self, fd: RawFd) -> io::Result<()> {
        while self.pid_report.is_some() {
            match sys::read(fd, &mut self.pid_buf[self.pid_len..]) {
                Ok(0) => self.pid_report = None,
                Ok(n) => {
                    self.pid_len += n;
                    if self.pid_len >= 4 {
                        self.container_pid = parse_pid_report(&self.pid_buf);
                        self.pid_report = None;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    fn poll_status(&mut self) -> io::Result<Option<i32>> {
        if self.status.is_none() {
            self.status = handle::wait_status(self.pid)?;
        }
        Ok(self.status)
    }

    fn emit_exit(&mut self, code: i32) -> Output {
        self.exit_sent = true;
        Output::Exited(code)
    }
}

impl Drop for ExecProc {
    fn drop(&mut self) {
        if handle::should_kill(self.status, self.exit_sent) {
            sys::kill(self.pid);
            let _ = sys::waitpid_blocking(self.pid);
        }
    }
}

/// Fork the two-level relay and hand back the exec'd process.
pub fn exec_into(spec: &ExecSpec) -> io::Result<ExecProc> {
    spec.validate()?;
    crate::ensure_single_threaded()?;
    // Everything opens in the parent so failures return before any fork.
    let mnt = open_ns(spec.pid, "mnt")?;
    let ipc = open_ns(spec.pid, "ipc")?;
    let uts = open_ns(spec.pid, "uts")?;
    let net = open_ns(spec.pid, "net")?;
    let cgroup = match open_ns(spec.pid, "cgroup") {
        Ok(fd) => Some(fd),
        // Kernels without cgroup namespaces simply have no file there.
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(e),
    };
    let pid_ns = open_ns(spec.pid, "pid")?;
    let root_path = sys::cstr(format!("/proc/{}/root", spec.pid))?;
    let root = sys::open_path(&root_path, libc::O_PATH | libc::O_DIRECTORY)?;
    let devnull = sys::open_path(c"/dev/null", libc::O_RDWR)?;
    let (stdout_r, stdout_w) = sys::pipe2()?;
    let (stderr_r, stderr_w) = sys::pipe2()?;
    let (pid_r, pid_w) = sys::pipe2()?;
    let (status_r, status_w) = sys::pipe2()?;
    let io = RelayIo {
        stdout_w: stdout_w.as_raw_fd(),
        stderr_w: stderr_w.as_raw_fd(),
        pid_w: pid_w.as_raw_fd(),
        status_w: status_w.as_raw_fd(),
        root: root.as_raw_fd(),
        devnull: devnull.as_raw_fd(),
        ns: [
            mnt.as_raw_fd(),
            ipc.as_raw_fd(),
            uts.as_raw_fd(),
            net.as_raw_fd(),
            cgroup.as_ref().map_or(-1, |f| f.as_raw_fd()),
            pid_ns.as_raw_fd(),
        ],
        reads: [
            stdout_r.as_raw_fd(),
            stderr_r.as_raw_fd(),
            pid_r.as_raw_fd(),
            status_r.as_raw_fd(),
        ],
    };
    let pid = match sys::fork() {
        Err(e) => return Err(e),
        Ok(0) => relay_a(spec, io),
        Ok(pid) => pid,
    };
    // Parent side: close every relay-held descriptor, then gate on the
    // exec report so pre-exec failures surface as a real io::Error.
    drop((
        stdout_w, stderr_w, pid_w, status_w, mnt, ipc, uts, net, cgroup, pid_ns, root, devnull,
    ));
    let reap = |pid: libc::pid_t| {
        sys::kill(pid);
        let _ = sys::waitpid_blocking(pid);
    };
    let report = read_exec_report(status_r.as_raw_fd(), 30_000).inspect_err(|_| reap(pid))?;
    drop(status_r);
    if let Some(text) = report {
        reap(pid);
        return Err(io::Error::other(text));
    }
    // A pipe that cannot go non-blocking leaves the handle unusable: kill
    // the relay instead of leaking a running exec.
    let nonblocking = (|| {
        sys::set_nonblocking(stdout_r.as_raw_fd())?;
        sys::set_nonblocking(stderr_r.as_raw_fd())?;
        sys::set_nonblocking(pid_r.as_raw_fd())
    })();
    if let Err(e) = nonblocking {
        reap(pid);
        return Err(e);
    }
    Ok(ExecProc {
        pid,
        stdout: stdout_r,
        stderr: stderr_r,
        stdout_open: true,
        stderr_open: true,
        pid_report: Some(pid_r),
        pid_buf: [0; 4],
        pid_len: 0,
        container_pid: None,
        status: None,
        exit_sent: false,
        grace: 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_argv_pid_and_cwd() {
        let mut s = ExecSpec::new(42, vec!["/bin/true".into()]);
        assert!(s.validate().is_ok());
        s.argv.clear();
        assert!(s.validate().is_err());
        let s = ExecSpec::new(0, vec!["/bin/true".into()]);
        assert!(s.validate().is_err());
        let mut s = ExecSpec::new(42, vec!["/bin/true".into()]);
        s.cwd = PathBuf::from("tmp");
        assert!(s.validate().is_err());
    }

    #[test]
    fn pid_report_parsing() {
        assert_eq!(parse_pid_report(&[]), None);
        assert_eq!(parse_pid_report(&[1, 2, 3]), None);
        assert_eq!(parse_pid_report(&[0xd0, 0x02, 0, 0]), Some(0x2d0));
        assert_eq!(parse_pid_report(&[1, 0, 0, 0, 9]), Some(1));
    }

    #[test]
    fn exec_ladder_skips_prefixes_for_paths() {
        let names: Vec<String> = candidates("sh")
            .iter()
            .map(|c| c.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            vec!["sh", "/bin/sh", "/usr/bin/sh", "/sbin/sh", "/usr/sbin/sh"]
        );
        assert_eq!(candidates("/bin/sh").len(), 1);
    }

    #[test]
    fn wait_timeout_reports_an_expired_budget_and_drop_reaps() {
        // Mirrors handle.rs: a plain sleep supplies only the pid (no
        // namespaces are entered); only the budget can end this wait, and
        // `Drop` must kill+reap the sleeper afterwards.
        let sleeper = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleeper");
        let (stdout_r, _) = sys::pipe2().expect("stdout pipe");
        let (stderr_r, _) = sys::pipe2().expect("stderr pipe");
        let (pid_r, pid_w) = sys::pipe2().expect("pid pipe");
        // Mirror `exec_into`: the polled readers MUST be non-blocking or
        // `poll_pid_report`'s read on the (still-empty) pid pipe blocks.
        for fd in [
            stdout_r.as_raw_fd(),
            stderr_r.as_raw_fd(),
            pid_r.as_raw_fd(),
        ] {
            sys::set_nonblocking(fd).expect("nonblocking fixture pipe");
        }
        let mut proc = ExecProc {
            pid: sleeper.id() as libc::pid_t,
            stdout: stdout_r,
            stderr: stderr_r,
            stdout_open: true,
            stderr_open: true,
            pid_report: Some(pid_r),
            pid_buf: [0; 4],
            pid_len: 0,
            container_pid: None,
            status: None,
            exit_sent: false,
            grace: 0,
        };
        let err = proc
            .wait_timeout(Duration::from_millis(10))
            .expect_err("must time out");
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        // The pid report is drained lazily: only inside next_event.
        assert_eq!(proc.container_pid(), None);
        sys::write_all(pid_w.as_raw_fd(), &7u32.to_le_bytes()).expect("write report");
        drop(pid_w);
        let _ = proc.next_event(Duration::from_millis(1)).expect("poll");
        assert_eq!(proc.container_pid(), Some(7));
        drop(proc);
        drop(sleeper);
    }
}
