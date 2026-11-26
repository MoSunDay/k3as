//! Streaming staging of a `docker save` tarball (TODO **T6.3** / **T8.1**,
//! the A1 long-run hardening of the offline import face).
//!
//! The importer never holds a layer in memory: the archive stream
//! (transparently gzip/zstd decoded) is walked ONCE, every regular member
//! is copied into a private staging directory while its SHA-256 is
//! computed (`hash-while-copy`), and only `manifest.json` -- a few KiB --
//! is kept in RAM. Peak RSS is therefore a fixed 256 KiB copy buffer plus
//! the config blob, regardless of the image size.
//!
//! The staging directory is owned by [`Staging`] and removed when the
//! import result is dropped, so a failed or completed Build leaves no
//! residue behind (B9 / C11 hygiene).

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use sha2::{Digest as _, Sha256};

use crate::tarimg::ImportLimits;

/// Name of the docker archive index entry.
pub(crate) const MANIFEST_ENTRY: &str = "manifest.json";
/// docker save also emits a legacy `repositories` JSON; unused here.
const REPOSITORIES_ENTRY: &str = "repositories";
/// Copy window: 256 KiB keeps RSS flat and syscalls cheap.
const COPY_BUF: usize = 256 * 1024;
///Magic bytes of the supported wrappers.
const GZIP_MAGIC: [u8; 2] = [0x1f, 0x8b];
const ZSTD_MAGIC: [u8; 4] = [0x28, 0xb5, 0x2f, 0xfd];

/// One staged regular-file member (`path` + `size` + `sha256:<hex>`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StagedEntry {
    pub path: PathBuf,
    pub size: u64,
    pub digest: String,
}

/// A private staging directory: `manifest.json` in RAM, every other member
/// on disk. `Drop` removes the whole directory tree.
#[derive(Debug)]
pub(crate) struct Staging {
    dir: PathBuf,
    pub manifest: Option<Vec<u8>>,
    pub entries: HashMap<String, StagedEntry>,
}

impl Drop for Staging {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Staging {
    /// The directory holding the staged members (removed on drop).
    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }
}

/// Allocate a unique staging directory under `root` (created on demand).
pub(crate) fn staging_dir(root: &Path) -> Result<PathBuf, String> {
    std::fs::create_dir_all(root)
        .map_err(|e| format!("create staging root {}: {e}", root.display()))?;
    static SEQ: AtomicU64 = AtomicU64::new(0);
    for _ in 0..64 {
        let dir = root.join(format!(
            "import-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        match std::fs::create_dir(&dir) {
            Ok(()) => return Ok(dir),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("create staging dir {}: {e}", dir.display())),
        }
    }
    Err("could not allocate a unique staging directory".to_string())
}

/// Open `path` and detect a gzip/zstd wrapper by magic bytes, returning a
/// decoded reader over the raw tar stream.
pub(crate) fn open_compressed(path: &Path) -> Result<Box<dyn Read>, String> {
    let mut file = File::open(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let mut magic = [0u8; 4];
    let n = file.read(&mut magic).unwrap_or(0);
    let chained: Box<dyn Read> = Box::new(io::Cursor::new(magic[..n].to_vec()).chain(file));
    if magic.starts_with(&GZIP_MAGIC) {
        return Ok(Box::new(flate2::read::GzDecoder::new(chained)));
    }
    if magic.starts_with(&ZSTD_MAGIC) {
        let decoder = zstd::stream::read::Decoder::new(chained)
            .map_err(|e| format!("zstd archive header: {e}"))?;
        return Ok(Box::new(decoder));
    }
    Ok(chained)
}

/// Budget reader: fails the stream as soon as the *uncompressed* archive
/// crosses `max_image_bytes` (compression-bomb + disk guard).
struct Capped<R> {
    inner: R,
    budget: u64,
    cap: u64,
}

impl<R: Read> Read for Capped<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        if n as u64 > self.budget {
            return Err(io::Error::new(
                io::ErrorKind::FileTooLarge,
                format!(
                    "docker save archive exceeds the {} byte import cap",
                    self.cap
                ),
            ));
        }
        self.budget -= n as u64;
        Ok(n)
    }
}

/// Stage every regular member of the archive; `manifest.json` stays in RAM
/// (bounded by `limits.max_metadata_bytes`), everything else lands in
/// `dir` with its digest computed while copying.
pub(crate) fn stage<R: Read>(
    reader: R,
    dir: PathBuf,
    limits: &ImportLimits,
) -> Result<Staging, String> {
    let mut staging = Staging {
        dir,
        manifest: None,
        entries: HashMap::new(),
    };
    let capped = Capped {
        inner: reader,
        budget: limits.max_image_bytes,
        cap: limits.max_image_bytes,
    };
    let mut archive = tar::Archive::new(capped);
    let entries = archive.entries().map_err(|e| format!("tar: {e}"))?;
    let mut index = 0u64;
    for entry in entries {
        let mut entry = entry.map_err(|e| format!("tar entry: {e}"))?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let raw = entry.path_bytes().into_owned();
        let name = normalize_entry_name(&String::from_utf8_lossy(&raw)).to_string();
        if name.is_empty() {
            continue;
        }
        if name == MANIFEST_ENTRY {
            let mut buf = Vec::new();
            entry
                .take(limits.max_metadata_bytes + 1)
                .read_to_end(&mut buf)
                .map_err(|e| format!("tar read {MANIFEST_ENTRY}: {e}"))?;
            if buf.len() as u64 > limits.max_metadata_bytes {
                return Err(format!(
                    "{MANIFEST_ENTRY} exceeds the {} byte import cap",
                    limits.max_metadata_bytes
                ));
            }
            staging.manifest = Some(buf);
            continue;
        }
        if name == REPOSITORIES_ENTRY || staging.entries.contains_key(&name) {
            continue; // unused legacy index / duplicate member: first wins
        }
        index += 1;
        let target = staging.dir.join(format!("{index:08}.blob"));
        let staged = copy_entry(&mut entry, &target, &name)?;
        staging.entries.insert(name, staged);
    }
    Ok(staging)
}

/// Copy one tar member into `target`, hashing while writing.
fn copy_entry<R: Read>(
    entry: &mut tar::Entry<'_, R>,
    target: &Path,
    name: &str,
) -> Result<StagedEntry, String> {
    let file = File::create(target).map_err(|e| format!("stage {name}: {e}"))?;
    let mut out = BufWriter::new(file);
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; COPY_BUF];
    let mut size = 0u64;
    loop {
        let n = entry
            .read(&mut buf)
            .map_err(|e| format!("tar read {name}: {e}"))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        out.write_all(&buf[..n])
            .map_err(|e| format!("stage {name}: {e}"))?;
        size += n as u64;
    }
    out.flush().map_err(|e| format!("stage {name}: {e}"))?;
    Ok(StagedEntry {
        path: target.to_path_buf(),
        size,
        digest: format!("sha256:{:x}", hasher.finalize()),
    })
}

/// Strip the `./` / leading-slash noise docker's tar writer adds.
pub(crate) fn normalize_entry_name(raw: &str) -> &str {
    let mut name = raw;
    while let Some(rest) = name.strip_prefix("./") {
        name = rest;
    }
    name.trim_start_matches('/')
}
