//! k3s-compatible runtime manifest format (ported from k3s
//! `pkg/dataverify/dataverify.go`).
//!
//! Two flat-text sidecar files describe a staged data directory:
//! - `.sha256sums` — standard `sha256sum -c` format: one record per line as
//!   `<64-hex-sha256>  <relative_path>` (exactly TWO spaces between hash and
//!   path, newline-terminated, sorted by path).
//! - `.links` — one symlink per line: `<target_path> <linkname_path>` (a single
//!   space, newline-terminated, sorted by linkname). `target` is relative to the
//!   data-dir root; `linkname` is the symlink path to create.
//!
//! Every function here is pure: no I/O, no global mutable state. Self-hash
//! verification ([`verify_sha256sums`]) flows through [`crate::digest`] so the
//! computation stays in-process and matches the rest of the acquire pipeline.

#![forbid(unsafe_code)]

use std::fmt;

use crate::digest;

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Error raised while parsing `.sha256sums` / `.links` content.
///
/// Carries a [`DataverifyErrorKind`], the 1-based line number, and a short
/// human-readable detail string. Construct via [`DataverifyError::new`].
#[derive(Debug)]
pub struct DataverifyError {
    kind: DataverifyErrorKind,
    line_no: usize,
    detail: String,
}

/// What went wrong while parsing a single record line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataverifyErrorKind {
    /// Line is non-empty / non-comment but structurally unparseable
    /// (e.g. only one field where two are required).
    MalformedLine,
    /// The hash field is present but is not 64 hex characters.
    InvalidHash,
    /// The line has the wrong number of whitespace-separated fields.
    WrongFieldCount,
}

impl DataverifyError {
    /// Build a new error referencing `line_no` (1-based).
    fn new(kind: DataverifyErrorKind, line_no: usize, detail: impl Into<String>) -> Self {
        Self {
            kind,
            line_no,
            detail: detail.into(),
        }
    }

    /// Categorisation of the failure.
    pub fn kind(&self) -> DataverifyErrorKind {
        self.kind
    }

    /// 1-based line number of the offending record.
    pub fn line_no(&self) -> usize {
        self.line_no
    }
}

impl fmt::Display for DataverifyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let label = match self.kind {
            DataverifyErrorKind::MalformedLine => "malformed line",
            DataverifyErrorKind::InvalidHash => "invalid hash",
            DataverifyErrorKind::WrongFieldCount => "wrong field count",
        };
        write!(
            f,
            "dataverify: {label} on line {}: {}",
            self.line_no, self.detail
        )
    }
}

impl std::error::Error for DataverifyError {}

// ---------------------------------------------------------------------------
// .sha256sums
// ---------------------------------------------------------------------------

/// Render `.sha256sums` content from `(path, sha256_hex)` pairs.
///
/// Entries are sorted by path before rendering; each line is
/// `<hash>  <path>` with exactly two spaces and a trailing newline. Empty
/// input yields an empty string.
pub fn render_sha256sums(entries: &[(String, String)]) -> String {
    let mut sorted: Vec<&(String, String)> = entries.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    let mut out = String::with_capacity(sorted.len() * 96);
    for (path, sha) in sorted {
        out.push_str(sha);
        out.push_str("  ");
        out.push_str(path);
        out.push('\n');
    }
    out
}

/// Parse `.sha256sums` content back into `(path, sha256)` pairs.
///
/// Blank lines and lines whose first non-whitespace character is `#` are
/// skipped. Each remaining line must be `<64-hex>  <path>`; the hash is
/// validated to be exactly 64 ASCII hex characters (case-insensitive).
pub fn parse_sha256sums(s: &str) -> Result<Vec<(String, String)>, DataverifyError> {
    let mut out = Vec::new();
    for (i, raw) in s.lines().enumerate() {
        let line_no = i + 1;
        if is_skippable(raw) {
            continue;
        }
        // Line layout: `<hash>  <path>` — first field is the hash.
        let (hash, path) = two_fields(raw, line_no)?;
        if !is_hex64(hash) {
            return Err(DataverifyError::new(
                DataverifyErrorKind::InvalidHash,
                line_no,
                format!("`{hash}` is not 64 hex characters"),
            ));
        }
        out.push((path.to_string(), hash.to_string()));
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// .links
// ---------------------------------------------------------------------------

/// Render `.links` content from `(target, linkname)` pairs.
///
/// Pairs are sorted by `linkname` before rendering; each line is
/// `<target> <linkname>` with a single space and a trailing newline. Empty
/// input yields an empty string.
pub fn render_links(links: &[(String, String)]) -> String {
    let mut sorted: Vec<&(String, String)> = links.iter().collect();
    sorted.sort_by(|a, b| a.1.cmp(&b.1));
    let mut out = String::with_capacity(sorted.len() * 96);
    for (target, linkname) in sorted {
        out.push_str(target);
        out.push(' ');
        out.push_str(linkname);
        out.push('\n');
    }
    out
}

/// Parse `.links` content back into `(target, linkname)` pairs.
///
/// Blank lines and lines whose first non-whitespace character is `#` are
/// skipped. Each remaining line must be exactly two whitespace-separated
/// fields: `<target> <linkname>`.
pub fn parse_links(s: &str) -> Result<Vec<(String, String)>, DataverifyError> {
    let mut out = Vec::new();
    for (i, raw) in s.lines().enumerate() {
        let line_no = i + 1;
        if is_skippable(raw) {
            continue;
        }
        let (target, linkname) = two_fields(raw, line_no)?;
        out.push((target.to_string(), linkname.to_string()));
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Self-verification
// ---------------------------------------------------------------------------

/// True iff the SHA-256 of `content`'s UTF-8 bytes equals `expected_hash`
/// (case-insensitive).
///
/// Used to self-verify the `.sha256sums` file itself. The hash is computed
/// in-process via [`crate::digest`]; a hash failure returns `false` rather
/// than an error.
pub fn verify_sha256sums(content: &str, expected_hash: &str) -> bool {
    match digest::sha256_reader(content.as_bytes()) {
        Ok(computed) => computed.eq_ignore_ascii_case(expected_hash),
        // `&[u8]` reads never error in practice; treat any failure as no-match.
        Err(_) => false,
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// True for lines that should be skipped during parsing (blank or `#`-comment).
fn is_skippable(line: &str) -> bool {
    let t = line.trim_start();
    t.is_empty() || t.starts_with('#')
}

/// True iff `s` is exactly 64 ASCII hex characters (case-insensitive).
fn is_hex64(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Split a record line into exactly two whitespace-separated fields.
///
/// Returns `(first, second)`. A single field yields [`MalformedLine`]
/// (incomplete record); three or more yields [`WrongFieldCount`].
///
/// [`MalformedLine`]: DataverifyErrorKind::MalformedLine
/// [`WrongFieldCount`]: DataverifyErrorKind::WrongFieldCount
fn two_fields(line: &str, line_no: usize) -> Result<(&str, &str), DataverifyError> {
    let mut it = line.split_whitespace();
    match (it.next(), it.next(), it.next()) {
        (Some(a), Some(b), None) => Ok((a, b)),
        (Some(_), None, _) => Err(DataverifyError::new(
            DataverifyErrorKind::MalformedLine,
            line_no,
            "expected two fields, found only one",
        )),
        _ => Err(DataverifyError::new(
            DataverifyErrorKind::WrongFieldCount,
            line_no,
            "expected exactly two whitespace-separated fields",
        )),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const H1: &str = "1111111111111111111111111111111111111111111111111111111111111111";
    const H2: &str = "2222222222222222222222222222222222222222222222222222222222222222";
    const EMPTY_SHA: &str =
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    #[test]
    fn render_sha256sums_format() {
        // Deliberately unsorted input.
        let entries = vec![
            ("bin/zzz".to_string(), H2.to_string()),
            ("bin/aaa".to_string(), H1.to_string()),
        ];
        let out = render_sha256sums(&entries);
        // Sorted by path, two-space separator, newline-terminated.
        assert_eq!(out, format!("{H1}  bin/aaa\n{H2}  bin/zzz\n"));
        assert!(out.contains("  bin/aaa\n"));
        assert!(out.ends_with('\n'));
    }

    #[test]
    fn parse_sha256sums_round_trip() {
        let entries = vec![
            ("bin/runc".to_string(), H1.to_string()),
            ("bin/aux/bridge".to_string(), H2.to_string()),
        ];
        let rendered = render_sha256sums(&entries);
        let parsed = parse_sha256sums(&rendered).unwrap();
        // Render sorts by path, so parse returns the sorted order.
        assert_eq!(
            parsed,
            vec![
                ("bin/aux/bridge".to_string(), H2.to_string()),
                ("bin/runc".to_string(), H1.to_string()),
            ]
        );
    }

    #[test]
    fn parse_sha256sums_skips_comments_and_blanks() {
        let s = format!("# header comment\n\n{H1}  bin/a\n   \n# trailing\n");
        let parsed = parse_sha256sums(&s).unwrap();
        assert_eq!(parsed, vec![("bin/a".to_string(), H1.to_string())]);
    }

    #[test]
    fn parse_sha256sums_rejects_non_hex() {
        let err = parse_sha256sums("zzzz  bin/a\n").unwrap_err();
        assert_eq!(err.kind(), DataverifyErrorKind::InvalidHash);
        assert_eq!(err.line_no(), 1);
        assert!(err.to_string().contains("invalid hash"), "{err}");
    }

    #[test]
    fn parse_sha256sums_rejects_wrong_length_hash() {
        let err = parse_sha256sums("abc123  bin/a\n").unwrap_err();
        assert_eq!(err.kind(), DataverifyErrorKind::InvalidHash);
        assert!(err.to_string().contains("not 64 hex"), "{err}");
    }

    #[test]
    fn parse_sha256sums_rejects_field_count() {
        // Three fields → WrongFieldCount.
        let err = parse_sha256sums(&format!("{H1}  bin/a  extra\n")).unwrap_err();
        assert_eq!(err.kind(), DataverifyErrorKind::WrongFieldCount);
        assert!(err.to_string().contains("field count"), "{err}");
    }

    #[test]
    fn parse_sha256sums_rejects_single_field() {
        // One field → MalformedLine.
        let err = parse_sha256sums(&format!("{H1}\n")).unwrap_err();
        assert_eq!(err.kind(), DataverifyErrorKind::MalformedLine);
        assert_eq!(err.line_no(), 1);
    }

    #[test]
    fn render_links_format() {
        // Unsorted input; output sorts by linkname (second element).
        let links = vec![
            ("target/zzz".to_string(), "link/aaa".to_string()),
            ("target/mmm".to_string(), "link/zzz".to_string()),
        ];
        let out = render_links(&links);
        assert_eq!(out, "target/zzz link/aaa\ntarget/mmm link/zzz\n");
        // Single-space separator: no double-space run anywhere.
        assert_eq!(out.matches("  ").count(), 0);
        assert!(out.ends_with('\n'));
    }

    #[test]
    fn parse_links_round_trip() {
        let links = vec![
            ("bin/runc".to_string(), "bin/runc-link".to_string()),
            ("bin/aux/bridge".to_string(), "bin/cni0".to_string()),
        ];
        let rendered = render_links(&links);
        let parsed = parse_links(&rendered).unwrap();
        // Sorted by linkname: "bin/cni0" < "bin/runc-link".
        assert_eq!(
            parsed,
            vec![
                ("bin/aux/bridge".to_string(), "bin/cni0".to_string()),
                ("bin/runc".to_string(), "bin/runc-link".to_string()),
            ]
        );
    }

    #[test]
    fn parse_links_skips_comments_and_blanks() {
        let s = "  # comment\n\nbin/t bin/l\n";
        let parsed = parse_links(s).unwrap();
        assert_eq!(parsed, vec![("bin/t".to_string(), "bin/l".to_string())]);
    }

    #[test]
    fn parse_links_rejects_field_count() {
        let err = parse_links("a b c\n").unwrap_err();
        assert_eq!(err.kind(), DataverifyErrorKind::WrongFieldCount);
    }

    #[test]
    fn empty_inputs_produce_empty_output() {
        assert_eq!(render_sha256sums(&[]), "");
        assert_eq!(render_links(&[]), "");
        assert!(parse_sha256sums("").unwrap().is_empty());
        assert!(parse_links("").unwrap().is_empty());
    }

    #[test]
    fn verify_sha256sums_matches() {
        // Known SHA-256 of the empty string.
        assert!(verify_sha256sums("", EMPTY_SHA));
        // Self-consistent: compute via digest, then verify round-trips.
        let content = "abc";
        let h = crate::digest::sha256_reader(content.as_bytes()).unwrap();
        assert_eq!(
            h,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert!(verify_sha256sums(content, &h));
        // Case-insensitive comparison.
        assert!(verify_sha256sums(content, &h.to_uppercase()));
    }

    #[test]
    fn verify_sha256sums_no_match() {
        assert!(!verify_sha256sums("non-empty content", EMPTY_SHA));
        assert!(!verify_sha256sums("", "deadbeef"));
    }

    #[test]
    fn error_display_is_informative() {
        let err = parse_sha256sums("nope  path\n").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("line 1"), "{msg}");
        assert!(msg.contains("invalid hash"), "{msg}");
    }
}
