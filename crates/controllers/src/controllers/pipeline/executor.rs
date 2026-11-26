//! Docker CLI executor for the ServicePipeline controller (TODO **T8.3**,
//! decision **Q56**).
//!
//! Q56 posture: the host runs the container daemon and the controller drives
//! the `docker` CLI; this module is that seam. Every invocation is bounded by
//! `tokio::time::timeout` and the child is `kill_on_drop`, so a hung daemon
//! can never wedge a reconcile pass forever -- a timeout is reported as data
//! ([`CmdOutput::timed_out`]) and the Build reconciler turns it terminal.
//!
//! Failures are values, not `Err`: spawn errors, non-zero exits and timeouts
//! all come back as a [`CmdOutput`] whose `log` already starts with the
//! rendered command line (so a failure status carries what was attempted).
//!
//! Bounded capture (batch D / B3): both pipes are drained in 8 KiB reads
//! and only the LAST [`OUTPUT_CAP`] bytes of each stream survive; the rest
//! is counted and discarded while the child is still running, so a build
//! that talks for gigabytes neither grows the controller's RSS (the old
//! `wait_with_output` buffered everything before trimming) nor blocks on a
//! full pipe.

use std::collections::VecDeque;
use std::io;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;

/// Outcome of one CLI invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CmdOutput {
    /// Exit status zero.
    pub success: bool,
    /// Exit code (`None` when killed by a signal / never spawned).
    pub code: Option<i32>,
    /// `$ <bin> <args>` header followed by combined stdout + stderr.
    pub log: String,
    /// The budget elapsed and the child was killed.
    pub timed_out: bool,
}

/// Run `bin` with `args`, bounded by `timeout`; never returns `Err`.
pub async fn run(bin: &str, args: &[String], timeout: Duration) -> CmdOutput {
    run_with(bin, args, timeout, OUTPUT_CAP).await
}

/// Bytes of ONE stream (stdout or stderr) that survive into the log: the
/// last `OUTPUT_CAP` bytes. 256 KiB is far beyond any useful docker error
/// and small enough that a status object stays bounded (`status::bounded_log`
/// applies the final 4 KiB tail on top).
pub const OUTPUT_CAP: usize = 256 * 1024;

/// Pipe read granularity.
const READ_CHUNK: usize = 8 * 1024;

/// The drained tail of one pipe: its last bytes plus how many bytes were
/// discarded ahead of them.
#[derive(Debug, Default)]
struct Capped {
    kept: VecDeque<u8>,
    dropped: usize,
}

impl Capped {
    /// Append one chunk, evicting the oldest bytes past `cap`.
    fn push(&mut self, chunk: &[u8], cap: usize) {
        self.kept.extend(chunk.iter().copied());
        let overflow = self.kept.len().saturating_sub(cap);
        if overflow > 0 {
            self.kept.drain(..overflow);
            self.dropped += overflow;
        }
    }

    /// Lossy UTF-8 text, prefixed with a marker when bytes were dropped.
    fn render(&self) -> String {
        let mut out = String::new();
        if self.dropped > 0 {
            out.push_str(&format!("...(truncated {} bytes)\n", self.dropped));
        }
        let bytes: Vec<u8> = self.kept.iter().copied().collect();
        out.push_str(&String::from_utf8_lossy(&bytes));
        out
    }
}

/// Drain one pipe to EOF in [`READ_CHUNK`] reads, keeping only its last
/// `cap` bytes. Discarding instead of stopping is what keeps a child that
/// talks past the ceiling from blocking on a full pipe.
async fn read_capped<R: AsyncRead + Unpin>(pipe: Option<R>, cap: usize) -> io::Result<Capped> {
    let mut capped = Capped::default();
    let Some(mut reader) = pipe else {
        return Ok(capped);
    };
    let mut buf = vec![0u8; READ_CHUNK];
    loop {
        let n = reader.read(&mut buf).await?;
        if n == 0 {
            return Ok(capped);
        }
        capped.push(&buf[..n], cap);
    }
}

/// [`run`] with an explicit per-stream capture ceiling `cap` (bytes).
pub async fn run_with(bin: &str, args: &[String], timeout: Duration, cap: usize) -> CmdOutput {
    let header = format!("$ {bin} {}", args.join(" ")).trim_end().to_string();
    let child = Command::new(bin)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn();
    let mut child = match child {
        Ok(child) => child,
        Err(e) => {
            return CmdOutput {
                success: false,
                code: None,
                log: format!("{header}\nspawn failed: {e}\n"),
                timed_out: false,
            };
        }
    };
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    // `child` MOVES into the future: when the budget elapses, dropping the
    // future drops the child and `kill_on_drop` reaps it.
    let collected = async move {
        let (out, err) = tokio::try_join!(read_capped(stdout, cap), read_capped(stderr, cap))?;
        let status = child.wait().await?;
        Ok::<_, io::Error>((out, err, status))
    };
    match tokio::time::timeout(timeout, collected).await {
        Err(_) => CmdOutput {
            success: false,
            code: None,
            log: format!("{header}\ntimed out after {}s\n", timeout.as_secs()),
            timed_out: true,
        },
        Ok(Err(e)) => CmdOutput {
            success: false,
            code: None,
            log: format!("{header}\nwait failed: {e}\n"),
            timed_out: false,
        },
        Ok(Ok((out, err, status))) => {
            let mut log = header;
            log.push('\n');
            log.push_str(&out.render());
            log.push_str(&err.render());
            CmdOutput {
                success: status.success(),
                code: status.code(),
                log,
                timed_out: false,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(script: &str) -> Vec<String> {
        vec!["-c".to_string(), script.to_string()]
    }

    #[tokio::test]
    async fn successful_command_reports_stdout_in_the_log() {
        let out = run("/bin/sh", &sh("echo hello"), Duration::from_secs(5)).await;
        assert!(out.success);
        assert_eq!(out.code, Some(0));
        assert!(!out.timed_out);
        assert!(
            out.log.starts_with("$ /bin/sh -c echo hello\n"),
            "log: {}",
            out.log
        );
        assert!(out.log.contains("hello"));
    }

    #[tokio::test]
    async fn non_zero_exit_is_reported_with_the_code_and_stderr() {
        let out = run(
            "/bin/sh",
            &sh("echo boom >&2; exit 3"),
            Duration::from_secs(5),
        )
        .await;
        assert!(!out.success);
        assert_eq!(out.code, Some(3));
        assert!(!out.timed_out);
        assert!(out.log.contains("boom"), "log: {}", out.log);
    }

    #[tokio::test]
    async fn elapsed_budget_marks_the_run_timed_out() {
        let out = run("/bin/sh", &sh("sleep 5"), Duration::from_millis(50)).await;
        assert!(!out.success);
        assert!(out.timed_out);
        assert!(out.log.contains("timed out"), "log: {}", out.log);
    }

    #[tokio::test]
    async fn missing_binary_is_a_spawn_failure() {
        let out = run("/nonexistent/init-pro-docker", &[], Duration::from_secs(1)).await;
        assert!(!out.success);
        assert!(!out.timed_out);
        assert!(out.log.contains("spawn failed"), "log: {}", out.log);
    }

    /// B3: the eviction math. Three 4-byte chunks against a 6-byte ceiling
    /// keep exactly the last 6 bytes and count the 6 evicted ones.
    #[test]
    fn capped_keeps_only_the_last_bytes() {
        let mut capped = Capped::default();
        for chunk in ["wxyz", "abcd", "efgh"] {
            capped.push(chunk.as_bytes(), 6);
        }
        let bytes: Vec<u8> = capped.kept.iter().copied().collect();
        assert_eq!(bytes, b"cdefgh");
        assert_eq!(capped.dropped, 6);
        assert!(capped.render().starts_with("...(truncated 6 bytes)\n"));
        let intact = Capped::default();
        assert_eq!(intact.render(), "", "no marker without dropped bytes");
    }

    /// B3: output is capped WHILE it streams. 2 MiB on stdout + 1 MiB on
    /// stderr against a 1 KiB ceiling leave two tails + two markers, and
    /// the child still exits 0 (the pipes are drained to EOF, never left
    /// full for the writer to block on).
    #[tokio::test]
    async fn huge_output_is_capped_while_streaming() {
        let script = "head -c 2097152 /dev/zero | tr '\\000' a; \
                      head -c 1048576 /dev/zero | tr '\\000' b >&2";
        let out = run_with("/bin/sh", &sh(script), Duration::from_secs(30), 1024).await;
        assert!(out.success, "log: {}", out.log);
        assert_eq!(out.code, Some(0));
        assert!(!out.timed_out);
        assert!(
            out.log.contains("...(truncated 2096128 bytes)"),
            "log: {}",
            out.log
        );
        assert!(
            out.log.contains("...(truncated 1047552 bytes)"),
            "log: {}",
            out.log
        );
        // header + 2 x (marker + 1 KiB tail), nothing else.
        let bound =
            (out.log.len()).saturating_sub(2 * 1024 + 2 * "...(truncated NNNNNNN bytes)\n".len());
        assert!(bound <= script.len() + 32, "log: {} bytes", out.log.len());
        assert!(
            out.log.ends_with(&"b".repeat(1024)),
            "the stderr tail did not survive"
        );
    }

    /// B3: a child that never stops talking is still killed by the budget.
    #[tokio::test]
    async fn elapsed_budget_kills_a_chatty_child() {
        let out = run_with(
            "/bin/sh",
            &sh("while true; do echo spam; done"),
            Duration::from_millis(150),
            4096,
        )
        .await;
        assert!(!out.success);
        assert!(out.timed_out);
        assert!(out.log.contains("timed out"), "log: {}", out.log);
    }
}
