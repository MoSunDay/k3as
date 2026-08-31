//! NFS volume mount/unmount for kubelet (T-anyhome, T4.3 Scope A, Q36).
//!
//! The `nfs` volume kind binds `host:/workflow/<wf>` (anyhome, Q36) into
//! the pod's per-uid volume tree. All execs go through [`run`], which
//! retries `ETXTBSY` ("Text file busy", os error 26): freshly written
//! fake binaries in tests (and, on some filesystems, real helpers) can
//! still be marked busy by the writer at first exec. Tests never mutate
//! the process env - they pass explicit fake-binary paths instead.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How many times [`run`] retries a spawn that failed with `ETXTBSY`,
/// and how long it waits between attempts.
const EXEC_BUSY_ATTEMPTS: usize = 4;
const EXEC_BUSY_BACKOFF: Duration = Duration::from_millis(25);

/// Mount a `server:path` export at `dir` with `bin` (prod: the bare
/// `mount.nfs` name so PATH lookup resolves; tests pass a fake script).
/// NFSv3 over TCP, no lock manager (anyhome serves none), same port for
/// the mount and NFS services when the server carries an explicit port.
pub async fn mount_nfs(bin: &Path, server: &str, path: &str, dir: &Path) -> Result<(), String> {
    let args = mount_args(server, path, dir);
    let output = run(bin, &args).map_err(|e| format!("spawn {}: {e}", bin.display()))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(format!(
        "{} {server}:{path} -> {} failed ({}): {}",
        bin.display(),
        dir.display(),
        output.status,
        stderr.trim()
    ))
}

/// Unmount every mount found under `root` in `mounts_text` (the text of
/// `/proc/self/mounts`), deepest first, so nested binds die before their
/// parents. A failed umount aborts with the helper's stderr; the caller
/// (volume cleanup) must then LEAVE the tree in place - deleting through
/// a live NFS mount would destroy server-side anyhome files.
pub async fn unmount_under(bin: &Path, mounts_text: &str, root: &Path) -> Result<(), String> {
    for mp in mountpoints_under(mounts_text, root) {
        let args = vec![mp.display().to_string()];
        let output = run(bin, &args).map_err(|e| format!("spawn {}: {e}", bin.display()))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!(
                "{} {} failed ({}): {}",
                bin.display(),
                mp.display(),
                output.status,
                stderr.trim()
            ));
        }
    }
    Ok(())
}

/// True when `dir` is itself a mountpoint per the mounts text.
pub fn is_mountpoint(mounts_text: &str, dir: &Path) -> bool {
    mount_paths(mounts_text)
        .iter()
        .any(|mp| mp == &dir.to_path_buf())
}

/// CLI shape for `mount.nfs`: `-o <opts> <host:path> <dir>`.
fn mount_args(server: &str, path: &str, dir: &Path) -> Vec<String> {
    vec![
        "-o".to_string(),
        mount_options(server),
        mount_source(server, path),
        dir.display().to_string(),
    ]
}

/// The mount SOURCE string, `host:path`. The transport port must NOT be
/// folded into the source: mount.nfs parses `host:port:/path` as hostname
/// `host` + dirpath `port:/path` and sends that dirpath verbatim, so the
/// mountd resolves the junk component `port:` and answers MNT3ERR_NOENT
/// (observed on the wire: dirpath `43141:/agent`). The port travels only
/// through `port=`/`mountport=` options (see [`mount_options`]).
fn mount_source(server: &str, path: &str) -> String {
    match nfs_port(server) {
        Some(_) => {
            let (host, _) = server.rsplit_once(':').unwrap_or((server, ""));
            format!("{host}:{path}")
        }
        None => format!("{server}:{path}"),
    }
}

/// NFSv3/TCP options; `port=`/`mountport=` pin both services to the
/// server's explicit port when it has one (anyhome: one port serves
/// both), otherwise the kernel probes (standard NFS ports).
fn mount_options(server: &str) -> String {
    let port_opts = match nfs_port(server) {
        Some(p) => format!(",port={p},mountport={p}"),
        None => String::new(),
    };
    format!("vers=3,tcp,nolock,noacl{port_opts}")
}

/// The port suffix of `host:port`, if present and numeric.
fn nfs_port(server: &str) -> Option<u16> {
    let (_, port) = server.rsplit_once(':')?;
    port.parse().ok()
}

/// Mountpoints under `root` (root itself included), deepest first - the
/// only safe unmount order for nested binds.
fn mountpoints_under(mounts_text: &str, root: &Path) -> Vec<PathBuf> {
    let mut mps: Vec<PathBuf> = mount_paths(mounts_text)
        .into_iter()
        .filter(|mp| mp == root || mp.starts_with(root))
        .collect();
    mps.sort_by_key(|mp| std::cmp::Reverse(mp.as_os_str().len()));
    mps
}

/// The second field (mount path) of each line, octal-unescaped.
fn mount_paths(mounts_text: &str) -> Vec<PathBuf> {
    mounts_text
        .lines()
        .filter_map(|line| line.split_whitespace().nth(1))
        .map(|raw| PathBuf::from(unescape_mnt(raw)))
        .collect()
}

/// Undo the kernel's `\0nn` octal escaping in `/proc/self/mounts`.
fn unescape_mnt(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 3 < bytes.len() {
            let oct = std::str::from_utf8(&bytes[i + 1..i + 4]).unwrap_or("");
            if let Ok(v) = u8::from_str_radix(oct, 8) {
                out.push(v);
                i += 4;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Spawn + wait, retrying `ETXTBSY` (see module docs).
fn run(bin: &Path, args: &[String]) -> std::io::Result<std::process::Output> {
    let mut last: Option<std::io::Error> = None;
    for attempt in 0..EXEC_BUSY_ATTEMPTS {
        match std::process::Command::new(bin).args(args).output() {
            Ok(output) => return Ok(output),
            Err(e) if attempt + 1 < EXEC_BUSY_ATTEMPTS && is_exec_busy(&e) => {
                last = Some(e);
                std::thread::sleep(EXEC_BUSY_BACKOFF);
            }
            Err(e) => return Err(e),
        }
    }
    Err(last.unwrap_or_else(|| std::io::Error::other("exec busy")))
}

/// `ETXTBSY` via the stable enum kind when available, plus the raw
/// errno/text fallback (error strings differ across libc messages).
fn is_exec_busy(e: &std::io::Error) -> bool {
    if e.kind() == ErrorKind::ExecutableFileBusy {
        return true;
    }
    e.raw_os_error() == Some(26) || {
        let text = e.to_string().to_ascii_lowercase();
        text.contains("text file busy")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "nfs-mount-{}-{}-{}",
            std::process::id(),
            tag,
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    /// A fake executable: `body` is a shell fragment run with "$@" set;
    /// `$0.args` next to the script records every invocation.
    fn fake_bin(tag: &str, body: &str) -> PathBuf {
        let dir = temp_dir(tag);
        let bin = dir.join(tag);
        let recorder = dir.join(format!("{tag}.args"));
        let script = format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" >> '{}'\n{body}\n",
            recorder.display()
        );
        std::fs::write(&bin, script).expect("write fake bin");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755))
            .expect("chmod fake bin");
        bin
    }

    fn recorded(bin: &Path) -> Vec<String> {
        let mut path = bin.to_path_buf();
        path.set_extension("args");
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn mount_options_default_without_port() {
        assert_eq!(mount_options("127.0.0.1"), "vers=3,tcp,nolock,noacl");
    }

    #[test]
    fn mount_options_pin_server_port() {
        assert_eq!(
            mount_options("127.0.0.1:11111"),
            "vers=3,tcp,nolock,noacl,port=11111,mountport=11111"
        );
    }

    #[test]
    fn mount_args_shape() {
        let args = mount_args("h:1", "/workflow/wf", Path::new("/mnt/x"));
        assert_eq!(args[0], "-o");
        assert_eq!(args[1], "vers=3,tcp,nolock,noacl,port=1,mountport=1");
        // Source is host-only; `h:1:/workflow/wf` would be sent to the
        // server as dirpath `1:/workflow/wf` and answered NOENT.
        assert_eq!(args[2], "h:/workflow/wf");
        assert_eq!(args[3], "/mnt/x");
    }

    #[test]
    fn mount_source_keeps_host_only_when_server_carries_port() {
        assert_eq!(
            mount_source("127.0.0.1:35289", "/agent"),
            "127.0.0.1:/agent"
        );
        // No port: verbatim. IPv6 literal without port: verbatim.
        assert_eq!(mount_source("127.0.0.1", "/workflow/w"), "127.0.0.1:/workflow/w");
        assert_eq!(mount_source("[::1]", "/agent"), "[::1]:/agent");
    }

    #[test]
    fn is_mountpoint_matches_exact_and_unescaped() {
        let mounts = "/dev/sda1 / ext4 rw\nsrv:/wf /tmp/a\\040b nfs3 rw\n";
        assert!(is_mountpoint(mounts, Path::new("/")));
        assert!(is_mountpoint(mounts, Path::new("/tmp/a b")));
        assert!(!is_mountpoint(mounts, Path::new("/tmp/a")));
    }

    #[test]
    fn mountpoints_under_orders_deepest_first_and_respects_boundary() {
        let mounts = "a /m nfs3 rw\nb /m/deep nfs3 rw\nc /mother nfs3 rw\n";
        let mps = mountpoints_under(mounts, Path::new("/m"));
        assert_eq!(mps, vec![PathBuf::from("/m/deep"), PathBuf::from("/m")]);
    }

    #[tokio::test]
    async fn mount_nfs_succeeds_on_zero_exit() {
        let bin = fake_bin("mnfs-ok", "exit 0\n");
        mount_nfs(&bin, "h:1", "/workflow/wf", Path::new("/mnt/x"))
            .await
            .expect("mount should succeed");
        let args = recorded(&bin);
        assert_eq!(args[2], "h:/workflow/wf");
        assert_eq!(args[3], "/mnt/x");
    }

    #[tokio::test]
    async fn mount_nfs_reports_stderr_on_nonzero_exit() {
        let bin = fake_bin("mnfs-bad", "echo 'fake mount: bad export' >&2\nexit 3\n");
        let err = mount_nfs(&bin, "h:1", "/workflow/wf", Path::new("/mnt/x"))
            .await
            .expect_err("mount should fail");
        assert!(err.contains("bad export"), "stderr lost: {err}");
        assert!(err.contains("/mnt/x"), "target lost: {err}");
    }

    #[tokio::test]
    async fn mount_nfs_missing_binary_is_reported() {
        let err = mount_nfs(
            Path::new("/nonexistent/mount.nfs"),
            "h:1",
            "/workflow/wf",
            Path::new("/mnt/x"),
        )
        .await
        .expect_err("spawn should fail");
        assert!(err.contains("spawn"), "unexpected: {err}");
    }

    #[tokio::test]
    async fn unmount_under_unmounts_deepest_first_only() {
        let bin = fake_bin("umnt-ok", "exit 0\n");
        let mounts = "a /root/mp nfs3 rw\nb /root/mp/deep nfs3 rw\nc /root/mpx nfs3 rw\n";
        unmount_under(&bin, mounts, Path::new("/root/mp"))
            .await
            .expect("umount should succeed");
        assert_eq!(recorded(&bin), vec!["/root/mp/deep", "/root/mp"]);
    }

    #[tokio::test]
    async fn unmount_under_propagates_failure_with_stderr() {
        let bin = fake_bin("umnt-bad", "echo 'device busy' >&2\nexit 16\n");
        let mounts = "a /root/mp nfs3 rw\n";
        let err = unmount_under(&bin, mounts, Path::new("/root/mp"))
            .await
            .expect_err("umount should fail");
        assert!(err.contains("device busy"), "stderr lost: {err}");
        assert!(err.contains("/root/mp"), "target lost: {err}");
    }

    #[test]
    fn exec_busy_matches_errno_and_text() {
        assert!(is_exec_busy(&std::io::Error::from_raw_os_error(26)));
        assert!(is_exec_busy(&std::io::Error::other(
            "Text file busy (os error 26)"
        )));
        assert!(!is_exec_busy(&std::io::Error::from_raw_os_error(2)));
    }
}
