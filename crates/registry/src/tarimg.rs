//! `docker save` tarball importer (TODO **T6.3**, M1 slice; the offline
//! ingest half of the T8.1 delivery-trust push face).
//!
//! The offline counterpart of the V2 push client: take the tar stream
//! `docker save` / `buildah bud --output type=docker` produce, pull out
//! the image config and its layer tars, and hand them to
//! [`to_push_image`] so `push_image` can publish them. gzip and zstd
//! wrappers are detected by magic bytes and decompressed transparently
//! (the same `flate2`/`zstd` stack the image unpacker uses, Q54).
//!
//! Layout parsed (docker archive, not OCI image layout):
//! `manifest.json` -> `[{Config, RepoTags, Layers}]`, plus the config
//! JSON and every `Layers` entry as regular tar members.
//!
//! **A1 memory posture:** the archive is staged to disk in ONE pass with
//! `hash-while-copy` ([`crate::tarstream`]); only `manifest.json` and the
//! image config stay in RAM. A 4 GiB image therefore costs a bounded
//! ~256 KiB of RSS instead of 2-3x its size, and a hostile archive that
//! crosses `max_image_bytes` is rejected before it can fill either RAM or
//! the disk (the staging directory is removed on drop).

use std::io::Read;
use std::path::{Path, PathBuf};

use crate::client::{BlobBody, LayerBlob, PushImage, CONFIG_MEDIA_TYPE, LAYER_MEDIA_TYPE};
use crate::tarstream::{self, Staging};

/// Default cap on the *uncompressed* archive stream (compression-bomb and
/// disk guard; the docker CLI saves images of this size in practice).
pub const DEFAULT_MAX_IMAGE_BYTES: u64 = 8 * 1024 * 1024 * 1024;
/// Default cap on `manifest.json` itself (a few KiB in practice).
pub const DEFAULT_MAX_METADATA_BYTES: u64 = 4 * 1024 * 1024;

/// Import budget: `max_image_bytes` bounds the decoded archive, and
/// `max_metadata_bytes` bounds `manifest.json` and the config blob.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImportLimits {
    pub max_image_bytes: u64,
    pub max_metadata_bytes: u64,
}

impl Default for ImportLimits {
    fn default() -> Self {
        Self {
            max_image_bytes: DEFAULT_MAX_IMAGE_BYTES,
            max_metadata_bytes: DEFAULT_MAX_METADATA_BYTES,
        }
    }
}

/// One layer of a staged image: a file on disk plus its size and digest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DockerLayer {
    /// Media type the push client advertises (uncompressed diff tar).
    pub media_type: String,
    /// Staged (already decoded) layer bytes on disk.
    pub path: PathBuf,
    /// Size in bytes.
    pub size: u64,
    /// `sha256:<64 hex>` over the staged bytes.
    pub digest: String,
}

/// One image out of a `docker save` archive, layers staged on disk.
#[derive(Debug)]
pub struct DockerSave {
    /// `RepoTags` (`["repo:tag", ...]`); may be empty for untagged saves.
    pub repo_tags: Vec<String>,
    /// The image config JSON blob (small; kept in RAM).
    pub config: Vec<u8>,
    /// Layer tars in manifest order (bottom first, uncompressed, on disk).
    pub layers: Vec<DockerLayer>,
    /// Owns the staging directory; removes it on drop.
    staging: Staging,
}

impl DockerSave {
    /// Root of the private staging directory holding this image's layers
    /// (exists until this value drops; useful for diagnostics/tests).
    pub fn staging_root(&self) -> &Path {
        self.staging.dir()
    }
}

/// Parse a `docker save` archive held in memory (raw, gzip or zstd tar).
///
/// Only the FIRST image of a multi-image archive is imported: the push
/// API publishes one `repo:tag` per call. Layers are staged under the
/// system temp dir (see [`parse_docker_save_with`] for a custom root).
pub fn parse_docker_save(bytes: &[u8]) -> Result<DockerSave, String> {
    parse_docker_save_with(bytes, &std::env::temp_dir(), &ImportLimits::default())
}

/// [`parse_docker_save`] with an explicit staging root and import budget.
pub fn parse_docker_save_with(
    bytes: &[u8],
    staging_root: &Path,
    limits: &ImportLimits,
) -> Result<DockerSave, String> {
    if bytes.is_empty() {
        return Err("docker save archive is empty".to_string());
    }
    import_reader(decode_bytes(bytes)?, staging_root, limits)
}

/// Read + parse a `docker save` archive from disk (default limits, layers
/// staged under the system temp dir).
pub fn parse_docker_save_file(path: &Path) -> Result<DockerSave, String> {
    parse_docker_save_file_with(path, &std::env::temp_dir(), &ImportLimits::default())
}

/// [`parse_docker_save_file`] with an explicit staging root and budget.
pub fn parse_docker_save_file_with(
    path: &Path,
    staging_root: &Path,
    limits: &ImportLimits,
) -> Result<DockerSave, String> {
    import_reader(tarstream::open_compressed(path)?, staging_root, limits)
}

/// Convert a parsed archive into the push client payload using docker
/// media types (uncompressed `diff.tar` layers, docker config JSON).
/// The layers stay on disk: `push_image` streams them in 256 KiB windows.
pub fn to_push_image(save: &DockerSave) -> PushImage {
    PushImage {
        config: save.config.clone(),
        config_media_type: CONFIG_MEDIA_TYPE.to_string(),
        layers: save
            .layers
            .iter()
            .map(|layer| LayerBlob {
                media_type: layer.media_type.clone(),
                body: BlobBody::File {
                    path: layer.path.clone(),
                    size: layer.size,
                    digest: layer.digest.clone(),
                },
            })
            .collect(),
    }
}

// ---- import plumbing (pure) -----------------------------------------------

/// Decode an in-memory archive into a tar reader (gzip/zstd by magic).
fn decode_bytes(bytes: &[u8]) -> Result<Box<dyn Read + '_>, String> {
    if bytes.starts_with(&[0x1f, 0x8b]) {
        return Ok(Box::new(flate2::read::GzDecoder::new(bytes)));
    }
    if bytes.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
        let decoder = zstd::stream::read::Decoder::new(bytes)
            .map_err(|e| format!("zstd archive header: {e}"))?;
        return Ok(Box::new(decoder));
    }
    Ok(Box::new(std::io::Cursor::new(bytes)))
}

/// Stage the archive, then resolve `manifest.json` into an image.
fn import_reader<R: Read>(
    reader: R,
    staging_root: &Path,
    limits: &ImportLimits,
) -> Result<DockerSave, String> {
    let mut staging = tarstream::stage(reader, tarstream::staging_dir(staging_root)?, limits)?;
    let raw_manifest = staging
        .manifest
        .take()
        .ok_or_else(|| format!("docker save archive has no {}", tarstream::MANIFEST_ENTRY))?;
    let doc: serde_json::Value = serde_json::from_slice(&raw_manifest)
        .map_err(|e| format!("{} is not valid JSON: {e}", tarstream::MANIFEST_ENTRY))?;
    let images = doc.as_array().ok_or_else(|| {
        format!(
            "{} must be a JSON array of images",
            tarstream::MANIFEST_ENTRY
        )
    })?;
    let first = images
        .first()
        .ok_or_else(|| format!("{} lists no images", tarstream::MANIFEST_ENTRY))?;

    let config_name = first["Config"]
        .as_str()
        .map(tarstream::normalize_entry_name)
        .filter(|n| !n.is_empty())
        .ok_or_else(|| format!("{} entry has no Config path", tarstream::MANIFEST_ENTRY))?;
    let layer_names: Vec<&str> = first["Layers"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(tarstream::normalize_entry_name))
                .collect()
        })
        .unwrap_or_default();
    if layer_names.is_empty() {
        return Err("docker save image has no layers".to_string());
    }
    let repo_tags: Vec<String> = first["RepoTags"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();

    let config_entry = take_entry(&mut staging, config_name)?;
    let config = read_bounded(&config_entry.path, limits.max_metadata_bytes)
        .map_err(|e| format!("docker save config {config_name}: {e}"))?;

    let mut layers = Vec::with_capacity(layer_names.len());
    for name in layer_names {
        let entry = take_entry(&mut staging, name)?;
        layers.push(DockerLayer {
            media_type: LAYER_MEDIA_TYPE.to_string(),
            path: entry.path,
            size: entry.size,
            digest: entry.digest,
        });
    }
    Ok(DockerSave {
        repo_tags,
        config,
        layers,
        staging,
    })
}

/// Take one staged member out of the staging map (the file itself lives on
/// until `DockerSave` drops).
fn take_entry(staging: &mut Staging, name: &str) -> Result<tarstream::StagedEntry, String> {
    staging.entries.remove(name).ok_or_else(|| {
        format!(
            "docker save archive is missing {name} (referenced by {})",
            tarstream::MANIFEST_ENTRY
        )
    })
}

/// Read a staged file, refusing more than `cap` bytes.
fn read_bounded(path: &Path, cap: u64) -> Result<Vec<u8>, String> {
    let file = std::fs::File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    let mut buf = Vec::new();
    file.take(cap + 1)
        .read_to_end(&mut buf)
        .map_err(|e| format!("read {}: {e}", path.display()))?;
    if buf.len() as u64 > cap {
        return Err(format!("exceeds the {cap} byte import cap"));
    }
    Ok(buf)
}
