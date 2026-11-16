//! The parent-side container handle (T4.11, Q48): `Child` + `Output`.
//!
//! Adapted from kiri `crates/kiri-init/src/linux/container.rs` @ 7846fe0
//! (Apache-2.0), parent half. [`Child`] owns the pid, the non-blocking
//! stdout/stderr pipes, the reaped exit status and the container cgroup;
//! [`super::spawn`] builds it after the start gate opens.

use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::PathBuf;
use std::time::Duration;

use crate::{cgroup, sys};

/// One stream event from a running container.
#[derive(Debug, PartialEq, Eq)]
pub enum Output {
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    Exited(i32),
}

/// Idle slices a reaped child may keep its capture pipes open (a grandchild
/// inherited them) before `Exited` is reported anyway. `wait` slices are
/// 100 ms, so the drain is bounded to roughly 300 ms.
const EXIT_GRACE_SLICES: u8 = 3;

/// Poll slice used by the blocking wait helpers.
const WAIT_SLICE: Duration = Duration::from_millis(100);

/// Drop-time predicate, kept a pure function so its truth table is testable
/// without a live pid: only a child that is neither reaped nor reported may
/// still be SIGKILLed (once reaped the pid can belong to another process).
fn should_kill(status: Option<i32>, exit_sent: bool) -> bool {
    status.is_none() && !exit_sent
}

/// A cloned container process owned by the engine.
pub struct Child {
    pub(crate) pid: libc::pid_t,
    pub(crate) stdout: OwnedFd,
    pub(crate) stderr: OwnedFd,
    pub(crate) stdout_open: bool,
    pub(crate) stderr_open: bool,
    pub(crate) status: Option<i32>,
    pub(crate) exit_sent: bool,
    pub(crate) grace: u8,
    pub(crate) cgroup_dir: Option<PathBuf>,
}

impl Child {
    pub fn pid(&self) -> u32 {
        self.pid as u32
    }

    pub fn cgroup_dir(&self) -> Option<&std::path::Path> {
        self.cgroup_dir.as_deref()
    }

    /// Next stdout/stderr chunk, or the one-shot process exit. `None` means
    /// the timeout elapsed with no event. Data is always delivered before
    /// `Exited`, even when the child is already a zombie.
    pub fn next_event(&mut self, timeout: Duration) -> io::Result<Option<Output>> {
        if self.exit_sent {
            return Ok(None);
        }
        let timeout_ms = timeout.as_millis().min(i32::MAX as u128) as i32;
        let mut fds: Vec<libc::pollfd> = Vec::new();
        if self.stdout_open {
            fds.push(libc::pollfd {
                fd: self.stdout.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            });
        }
        if self.stderr_open {
            fds.push(libc::pollfd {
                fd: self.stderr.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            });
        }
        if fds.is_empty() {
            let status = self.poll_status()?;
            if let Some(code) = status {
                return Ok(Some(self.emit_exit(code)));
            }
            std::thread::sleep(timeout.min(Duration::from_millis(10)));
            return Ok(None);
        }
        sys::poll(&mut fds, timeout_ms)?;
        let mut found: Option<Output> = None;
        let mut index = 0;
        if self.stdout_open {
            if fds[index].revents != 0 {
                found = read_chunk(self.stdout.as_raw_fd(), true, &mut self.stdout_open)?;
            }
            index += 1;
        }
        if found.is_none() && self.stderr_open && fds[index].revents != 0 {
            found = read_chunk(self.stderr.as_raw_fd(), false, &mut self.stderr_open)?;
        }
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
        // The direct child is reaped but a grandchild still holds a pipe
        // open. Bound the drain instead of waiting for an EOF that never
        // comes: after a few idle slices report the exit anyway.
        if let Some(code) = status {
            if self.grace >= EXIT_GRACE_SLICES {
                return Ok(Some(self.emit_exit(code)));
            }
            self.grace = self.grace.saturating_add(1);
        }
        Ok(None)
    }

    /// Bounded wait that drains stdout/stderr as `Exited` is approached.
    pub fn wait_timeout(&mut self, budget: Duration) -> io::Result<i32> {
        if self.exit_sent {
            // Already reported once; waiting again cannot produce a code.
            return Err(io::Error::other("the child exit was already reported"));
        }
        let deadline = std::time::Instant::now().checked_add(budget);
        let slice = budget.min(WAIT_SLICE);
        loop {
            if let Some(Output::Exited(code)) = self.next_event(slice)? {
                return Ok(code);
            }
            if deadline.is_none_or(|d| std::time::Instant::now() >= d) {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "container did not exit",
                ));
            }
        }
    }

    /// Blocking wait that drains stdout/stderr as Exited is approached.
    pub fn wait(&mut self) -> io::Result<i32> {
        self.wait_timeout(Duration::from_secs(3600))
    }

    /// Make both capture pipes non-blocking; a failure here means the
    /// handle is unusable, so callers drop it (and `Drop` cleans up).
    pub(crate) fn set_nonblocking(&mut self) -> io::Result<()> {
        sys::set_nonblocking(self.stdout.as_raw_fd())?;
        sys::set_nonblocking(self.stderr.as_raw_fd())
    }

    pub fn kill(&mut self) {
        sys::kill(self.pid);
    }

    /// SIGKILL may only reach a child that is still ours and unreported;
    /// once it is reaped the pid can be recycled.
    fn needs_kill(&self) -> bool {
        should_kill(self.status, self.exit_sent)
    }

    /// Remove the container cgroup (call after `Exited`).
    pub fn cleanup(&mut self) {
        if let Some(dir) = self.cgroup_dir.take() {
            cgroup::remove(&dir);
        }
    }

    fn poll_status(&mut self) -> io::Result<Option<i32>> {
        if self.status.is_none() {
            match sys::waitpid_nohang(self.pid) {
                Ok(Some(code)) => self.status = Some(code),
                Ok(None) => {}
                Err(e) if e.raw_os_error() == Some(libc::ECHILD) => self.status = Some(255),
                Err(e) => return Err(e),
            }
        }
        Ok(self.status)
    }

    fn emit_exit(&mut self, code: i32) -> Output {
        self.exit_sent = true;
        Output::Exited(code)
    }
}

impl Drop for Child {
    fn drop(&mut self) {
        if self.needs_kill() {
            self.kill();
            let _ = sys::waitpid_blocking(self.pid);
        }
        self.cleanup();
    }
}

fn read_chunk(fd: libc::c_int, stdout: bool, open: &mut bool) -> io::Result<Option<Output>> {
    let mut buf = [0u8; 8192];
    match sys::read(fd, &mut buf) {
        Ok(0) => {
            *open = false;
            Ok(None)
        }
        Ok(n) => {
            let bytes = buf[..n].to_vec();
            Ok(Some(if stdout {
                Output::Stdout(bytes)
            } else {
                Output::Stderr(bytes)
            }))
        }
        Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(None),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kill_is_only_needed_for_a_live_unreported_child() {
        assert!(should_kill(None, false));
        assert!(!should_kill(Some(0), false));
        assert!(!should_kill(Some(7), false));
        assert!(!should_kill(None, true));
        assert!(!should_kill(Some(7), true));
    }

    /// A `Child` whose direct child was already reaped but whose pipes are
    /// still held open by a "grandchild" (the write ends stay alive here).
    fn reaped_with_open_pipes() -> (Child, OwnedFd, OwnedFd) {
        let (stdout_r, stdout_w) = crate::sys::pipe2().expect("stdout pipe");
        let (stderr_r, stderr_w) = crate::sys::pipe2().expect("stderr pipe");
        let child = Child {
            // Never signalled: `status` is set, so `Drop`/`poll_status` skip
            // the pid entirely.
            pid: i32::MAX,
            stdout: stdout_r,
            stderr: stderr_r,
            stdout_open: true,
            stderr_open: true,
            status: Some(7),
            exit_sent: false,
            grace: 0,
            cgroup_dir: None,
        };
        (child, stdout_w, stderr_w)
    }

    #[test]
    fn exit_is_reported_even_when_a_grandchild_holds_the_pipes() {
        let (mut child, _stdout_w, _stderr_w) = reaped_with_open_pipes();
        let code = child
            .wait_timeout(Duration::from_secs(5))
            .expect("bounded drain");
        assert_eq!(code, 7);
    }

    #[test]
    fn data_is_delivered_before_the_forced_exit() {
        let (mut child, stdout_w, _stderr_w) = reaped_with_open_pipes();
        // Two idle slices burn most of the grace budget...
        for _ in 0..2 {
            assert_eq!(child.next_event(Duration::from_millis(1)).unwrap(), None);
        }
        // ...but real data still wins over the forced `Exited`.
        crate::sys::write_all(stdout_w.as_raw_fd(), b"late").expect("write");
        match child.next_event(Duration::from_millis(10)).unwrap() {
            Some(Output::Stdout(bytes)) => assert_eq!(bytes, b"late"),
            other => panic!("expected stdout, got {other:?}"),
        }
    }

    #[test]
    fn wait_timeout_reports_an_expired_budget() {
        // A live child that never exits inside the budget (a plain `sleep`
        // supplies only the pid; the nsrun clone path is not involved), so
        // only the budget can end this wait.
        let sleeper = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleeper");
        let (stdout_r, _stdout_w) = crate::sys::pipe2().expect("stdout pipe");
        let (stderr_r, _stderr_w) = crate::sys::pipe2().expect("stderr pipe");
        let mut child = Child {
            pid: sleeper.id() as libc::pid_t,
            stdout: stdout_r,
            stderr: stderr_r,
            stdout_open: true,
            stderr_open: true,
            status: None,
            exit_sent: false,
            grace: 0,
            cgroup_dir: None,
        };
        let err = child
            .wait_timeout(Duration::from_millis(10))
            .expect_err("must time out");
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        // Dropping the handle SIGKILLs and reaps the sleeper (`needs_kill`).
        drop(child);
        drop(sleeper);
    }
}
