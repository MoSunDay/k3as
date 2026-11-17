//! CRI container-log lines (T4.12, Q50): the engine appends
//! `<RFC3339Nano> <stdout|stderr> <P|F> <line>` text to the kubelet-owned
//! log path with O_APPEND, the same on-disk shape containerd writes for
//! `kubectl logs`.
//!
//! `F` marks a full line (newline seen), `P` a partial one flushed at
//! exit or by the size cap. One writer per container run; exec output
//! stays off the file (it is connection-scoped, Q50).

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

/// A partial line larger than this is flushed as `P` so a chatty
/// newline-free process cannot grow the buffer without bound.
const MAX_PENDING: usize = 1024 * 1024;

/// One CRI log line, pure so the exact bytes are testable.
pub(crate) fn line(timestamp: &str, stderr: bool, partial: bool, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + timestamp.len() + 16);
    out.extend_from_slice(timestamp.as_bytes());
    out.push(b' ');
    out.extend_from_slice(if stderr { b"stderr" } else { b"stdout" });
    out.push(b' ');
    out.push(if partial { b'P' } else { b'F' });
    out.push(b' ');
    out.extend_from_slice(data);
    out.push(b'\n');
    out
}

/// Current timestamp in the fixed 9-digit fraction format.
fn now_stamp() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    common::time::now_rfc3339_nano(now.as_secs(), now.subsec_nanos())
}

/// Append-only CRI line writer over the kubelet log path.
pub(crate) struct LogWriter {
    file: File,
    /// Held partial line plus the stream it came from: the two streams
    /// share one file, so a held partial must never be mislabeled by a
    /// chunk of the other stream (Q50).
    pending: Option<(bool, Vec<u8>)>,
}

impl LogWriter {
    /// Opens (creating parents) with O_APPEND so a restarted engine's
    /// lines can never overwrite earlier ones.
    pub fn open(path: &Path) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self {
            file,
            pending: None,
        })
    }

    /// Feeds one stdout/stderr chunk; emits an `F` line per newline.
    pub fn chunk(&mut self, stderr: bool, bytes: &[u8]) -> io::Result<()> {
        if let Some((held_stream, held)) = self.pending.as_ref() {
            if *held_stream != stderr && !held.is_empty() {
                // Stream switch: flush the held partial of the previous
                // stream as `P` first so the file keeps total output
                // order (the two streams share one relay, Q50).
                let (prev, body) = self.pending.take().expect("held above");
                let stamp = now_stamp();
                self.file.write_all(&line(&stamp, prev, true, &body))?;
            }
        }
        let (_, pending) = self.pending.get_or_insert_with(|| (stderr, Vec::new()));
        pending.extend_from_slice(bytes);
        while let Some(i) = pending.iter().position(|b| *b == b'\n') {
            let data: Vec<u8> = pending.drain(..=i).collect();
            // Drop the trailing newline: it is the line separator, and
            // the CRI format adds its own.
            let body = &data[..data.len() - 1];
            let stamp = now_stamp();
            self.file.write_all(&line(&stamp, stderr, false, body))?;
        }
        if pending.len() > MAX_PENDING {
            let (_, body) = self.pending.take().expect("held above");
            let stamp = now_stamp();
            self.file.write_all(&line(&stamp, stderr, true, &body))?;
        }
        Ok(())
    }

    /// Flushes a trailing partial line as `P` (container exit), tagged
    /// with the stream it actually came from.
    pub fn finish(&mut self) -> io::Result<()> {
        let Some((stream, body)) = self.pending.take() else {
            return Ok(());
        };
        if body.is_empty() {
            return Ok(());
        }
        let stamp = now_stamp();
        self.file.write_all(&line(&stamp, stream, true, &body))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wl(ts: &str, stderr: bool, partial: bool, data: &[u8]) -> String {
        String::from_utf8(line(ts, stderr, partial, data)).unwrap()
    }

    #[test]
    fn line_shape_matches_cri() {
        assert_eq!(
            wl("2026-11-17T00:00:00.000000001Z", false, false, b"hi"),
            "2026-11-17T00:00:00.000000001Z stdout F hi\n"
        );
        assert_eq!(wl("t", true, true, b"x"), "t stderr P x\n");
    }

    #[test]
    fn chunk_splits_full_lines_and_finish_flushes_partial() {
        let dir = std::env::temp_dir().join(format!("engine-logs-{}", std::process::id()));
        let path = dir.join("c.log");
        let _ = std::fs::remove_file(&path);
        let mut w = LogWriter::open(&path).unwrap();
        w.chunk(false, b"one\ntwo").unwrap();
        w.chunk(true, b"warn\n").unwrap();
        w.finish().unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].ends_with(" stdout F one"));
        assert!(lines[1].ends_with(" stdout P two"));
        assert!(lines[2].ends_with(" stderr F warn"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn append_never_truncates() {
        let dir = std::env::temp_dir().join(format!("engine-logs2-{}", std::process::id()));
        let path = dir.join("a.log");
        let _ = std::fs::remove_file(&path);
        {
            let mut w = LogWriter::open(&path).unwrap();
            w.chunk(false, b"first\n").unwrap();
        }
        {
            let mut w = LogWriter::open(&path).unwrap();
            w.chunk(false, b"second\n").unwrap();
        }
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.ends_with(" stdout F second\n"));
        assert_eq!(text.lines().count(), 2);
        let _ = std::fs::remove_file(&path);
    }
}
