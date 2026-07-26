//! SHA-256 helpers for content verification (k3s `sha256sum -c` parity).
//!
//! Pure-Rust via `sha2` so verification is in-process and testable; no shell
//! dependency on `sha256sum`. The build-time pin (Q6) and the runtime stage
//! manifest (B5) both flow through here.

#![forbid(unsafe_code)]

use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::Path;

/// Hex SHA-256 of a reader, streamed in 64 KiB chunks (no full buffer).
pub fn sha256_reader<R: Read>(mut r: R) -> std::io::Result<String> {
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = r.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex(&hasher.finalize()))
}

/// Hex SHA-256 of a file's bytes.
pub fn sha256_file(path: &Path) -> std::io::Result<String> {
    let f = std::fs::File::open(path)?;
    sha256_reader(f)
}

/// True iff the file's SHA-256 matches `expected_hex` (case-insensitive).
pub fn verify_file(path: &Path, expected_hex: &str) -> std::io::Result<bool> {
    Ok(sha256_file(path)?.eq_ignore_ascii_case(expected_hex))
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    const EMPTY: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    #[test]
    fn empty_reader_hash() {
        assert_eq!(sha256_reader(Cursor::new(b"")).unwrap(), EMPTY);
    }

    #[test]
    fn known_content_hash() {
        assert_eq!(
            sha256_reader(Cursor::new(b"abc")).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn verify_file_round_trip() {
        let dir = std::env::temp_dir().join("initpro-vendor-digest-test");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("f.bin");
        std::fs::write(&p, b"hello").unwrap();
        let h = sha256_file(&p).unwrap();
        assert!(verify_file(&p, &h).unwrap());
        assert!(!verify_file(&p, "deadbeef").unwrap());
        std::fs::remove_dir_all(&dir).ok();
    }
}
