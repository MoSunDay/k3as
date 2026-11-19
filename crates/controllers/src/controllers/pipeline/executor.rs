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

use std::time::Duration;

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
    let header = format!("$ {bin} {}", args.join(" ")).trim_end().to_string();
    let child = Command::new(bin)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn();
    let child = match child {
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
    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        // Dropping `wait_with_output` drops the child: `kill_on_drop` reaps it.
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
        Ok(Ok(out)) => {
            let mut log = header;
            log.push('\n');
            log.push_str(&String::from_utf8_lossy(&out.stdout));
            log.push_str(&String::from_utf8_lossy(&out.stderr));
            CmdOutput {
                success: out.status.success(),
                code: out.status.code(),
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
}
