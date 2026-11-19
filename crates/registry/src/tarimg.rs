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
//! JSON and every `Layers` entry as regular tar members. Memory is
//! bounded: the unpacked archive is capped at 8 GiB and the collected
//! config + layers at the same figure, so a hostile `.tar.gz` bomb
//! fails with a clear error instead of exhausting RAM.

use std::borrow::Cow;
use std::io::Read;
use std::path::Path;

use crate::client::{LayerBlob, PushImage, CONFIG_MEDIA_TYPE, LAYER_MEDIA_TYPE};

/// Name of the docker archive index entry.
const MANIFEST_ENTRY: &str = "manifest.json";
/// Cap on the unpacked tar stream (compression-bomb guard).
const MAX_UNPACKED_BYTES: u64 = 8 * 1024 * 1024 * 1024;
/// Cap on `manifest.json` itself (a few KiB in practice).
const MAX_METADATA_BYTES: u64 = 4 * 1024 * 1024;
const GZIP_MAGIC: [u8; 2] = [0x1f, 0x8b];
const ZSTD_MAGIC: [u8; 4] = [0x28, 0xb5, 0x2f, 0xfd];

/// One image out of a `docker save` archive, still as raw blobs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DockerSave {
    /// `RepoTags` (`["repo:tag", ...]`); may be empty for untagged saves.
    pub repo_tags: Vec<String>,
    /// The image config JSON blob.
    pub config: Vec<u8>,
    /// Layer tars in manifest order (bottom first, uncompressed).
    pub layers: Vec<Vec<u8>>,
}

/// Parse a `docker save` archive held in memory (raw, gzip or zstd tar).
///
/// Only the FIRST image of a multi-image archive is imported: the push
/// API publishes one `repo:tag` per call.
pub fn parse_docker_save(bytes: &[u8]) -> Result<DockerSave, String> {
    if bytes.is_empty() {
        return Err("docker save archive is empty".to_string());
    }
    let tar = tar_view(bytes)?;
    let raw_manifest = read_entry(&tar, MANIFEST_ENTRY, MAX_METADATA_BYTES)?
        .ok_or_else(|| format!("docker save archive has no {MANIFEST_ENTRY}"))?;
    let doc: serde_json::Value = serde_json::from_slice(&raw_manifest)
        .map_err(|e| format!("{MANIFEST_ENTRY} is not valid JSON: {e}"))?;
    let images = doc
        .as_array()
        .ok_or_else(|| format!("{MANIFEST_ENTRY} must be a JSON array of images"))?;
    let first = images
        .first()
        .ok_or_else(|| format!("{MANIFEST_ENTRY} lists no images"))?;

    let config_name = first["Config"]
        .as_str()
        .map(normalize_entry_name)
        .filter(|n| !n.is_empty())
        .ok_or_else(|| format!("{MANIFEST_ENTRY} entry has no Config path"))?;
    let layer_names: Vec<&str> = first["Layers"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(normalize_entry_name))
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

    let config = read_entry(&tar, config_name, MAX_UNPACKED_BYTES)?
        .ok_or_else(|| format!("docker save archive is missing config {config_name}"))?;
    let mut layers = Vec::with_capacity(layer_names.len());
    for name in layer_names {
        let layer = read_entry(&tar, name, MAX_UNPACKED_BYTES)?
            .ok_or_else(|| format!("docker save archive is missing layer {name}"))?;
        layers.push(layer);
    }

    let total: u64 = config.len() as u64 + layers.iter().map(|l| l.len() as u64).sum::<u64>();
    if total > MAX_UNPACKED_BYTES {
        return Err(format!(
            "docker save image exceeds the {MAX_UNPACKED_BYTES} byte import cap"
        ));
    }
    Ok(DockerSave {
        repo_tags,
        config,
        layers,
    })
}

/// Read + parse a `docker save` archive from disk.
pub fn parse_docker_save_file(path: &Path) -> Result<DockerSave, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    parse_docker_save(&bytes)
}

/// Convert a parsed archive into the push client payload using docker
/// media types (uncompressed `diff.tar` layers, docker config JSON).
pub fn to_push_image(save: &DockerSave) -> PushImage {
    PushImage {
        config: save.config.clone(),
        config_media_type: CONFIG_MEDIA_TYPE.to_string(),
        layers: save
            .layers
            .iter()
            .map(|data| LayerBlob {
                media_type: LAYER_MEDIA_TYPE.to_string(),
                data: data.clone(),
            })
            .collect(),
    }
}

// ---- tar plumbing (pure) --------------------------------------------------

/// The tar stream inside `bytes`, decompressed when a gzip/zstd wrapper
/// is detected. Borrows the input for plain tars (no copy).
fn tar_view(bytes: &[u8]) -> Result<Cow<'_, [u8]>, String> {
    if bytes.starts_with(&GZIP_MAGIC) {
        return Ok(Cow::Owned(read_capped(
            flate2::read::GzDecoder::new(bytes),
            "gzip",
        )?));
    }
    if bytes.starts_with(&ZSTD_MAGIC) {
        let decoder = zstd::stream::read::Decoder::new(bytes)
            .map_err(|e| format!("zstd archive header: {e}"))?;
        return Ok(Cow::Owned(read_capped(decoder, "zstd")?));
    }
    Ok(Cow::Borrowed(bytes))
}

/// Read `reader` to the end, refusing more than [`MAX_UNPACKED_BYTES`].
fn read_capped<R: Read>(reader: R, kind: &str) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    reader
        .take(MAX_UNPACKED_BYTES + 1)
        .read_to_end(&mut out)
        .map_err(|e| format!("{kind} decompress: {e}"))?;
    if out.len() as u64 > MAX_UNPACKED_BYTES {
        return Err(format!("{kind} archive exceeds the 8 GiB import cap"));
    }
    Ok(out)
}

/// Strip the `./` / leading-slash noise docker's tar writer adds.
fn normalize_entry_name(raw: &str) -> &str {
    let mut name = raw;
    while let Some(rest) = name.strip_prefix("./") {
        name = rest;
    }
    name.trim_start_matches('/')
}

/// Fetch one regular-file member of `tar` (first match wins), bounded to
/// `cap` bytes.
fn read_entry(tar: &[u8], want: &str, cap: u64) -> Result<Option<Vec<u8>>, String> {
    let mut archive = tar::Archive::new(std::io::Cursor::new(tar));
    let entries = archive.entries().map_err(|e| format!("tar: {e}"))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("tar entry: {e}"))?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let path = entry
            .path()
            .map_err(|e| format!("tar entry path: {e}"))?
            .to_string_lossy()
            .into_owned();
        if normalize_entry_name(&path) != want {
            continue;
        }
        let mut buf = Vec::new();
        entry
            .take(cap + 1)
            .read_to_end(&mut buf)
            .map_err(|e| format!("tar read {want}: {e}"))?;
        if buf.len() as u64 > cap {
            return Err(format!("docker save entry {want} exceeds the import cap"));
        }
        return Ok(Some(buf));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tar_with(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut b = tar::Builder::new(&mut out);
            for (name, data) in entries {
                let mut h = tar::Header::new_gnu();
                h.set_size(data.len() as u64);
                h.set_mode(0o644);
                h.set_cksum();
                b.append_data(&mut h, name, std::io::Cursor::new(*data))
                    .unwrap();
            }
            b.finish().unwrap();
        }
        out
    }

    const CONFIG: &[u8] = br#"{"architecture":"amd64","os":"linux"}"#;
    const LAYER: &[u8] = b"layer-tar-bytes";
    const MANIFEST: &[u8] = br#"[{"Config":"cfg.json","RepoTags":["example.com/demo:v1"],"Layers":["layer/layer.tar"]}]"#;

    fn synth(gzip: bool) -> Vec<u8> {
        let tar = tar_with(&[
            ("manifest.json", MANIFEST),
            ("cfg.json", CONFIG),
            ("layer/layer.tar", LAYER),
        ]);
        if !gzip {
            return tar;
        }
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut enc, &tar).unwrap();
        enc.finish().unwrap()
    }

    #[test]
    fn parses_plain_docker_save() {
        let save = parse_docker_save(&synth(false)).unwrap();
        assert_eq!(save.repo_tags, vec!["example.com/demo:v1".to_string()]);
        assert_eq!(save.config, CONFIG.to_vec());
        assert_eq!(save.layers, vec![LAYER.to_vec()]);

        let image = to_push_image(&save);
        assert_eq!(image.config_media_type, CONFIG_MEDIA_TYPE);
        assert_eq!(image.layers.len(), 1);
        assert_eq!(image.layers[0].media_type, LAYER_MEDIA_TYPE);
        assert_eq!(image.layers[0].data, LAYER.to_vec());
    }

    #[test]
    fn parses_gzip_and_zstd_wrapped_archives() {
        let plain = parse_docker_save(&synth(false)).unwrap();
        assert_eq!(parse_docker_save(&synth(true)).unwrap(), plain);

        let tar = tar_with(&[
            ("manifest.json", MANIFEST),
            ("cfg.json", CONFIG),
            ("layer/layer.tar", LAYER),
        ]);
        let z = zstd::stream::encode_all(std::io::Cursor::new(&tar), 3).unwrap();
        assert_eq!(parse_docker_save(&z).unwrap(), plain);
    }

    #[test]
    fn tolerates_dot_slash_entry_names() {
        let tar = tar_with(&[
            ("./manifest.json", MANIFEST),
            ("./cfg.json", CONFIG),
            ("./layer/layer.tar", LAYER),
        ]);
        assert_eq!(parse_docker_save(&tar).unwrap().layers.len(), 1);
    }

    #[test]
    fn rejects_malformed_archives() {
        assert!(parse_docker_save(b"").unwrap_err().contains("empty"));

        let no_manifest = tar_with(&[("cfg.json", CONFIG)]);
        assert!(parse_docker_save(&no_manifest)
            .unwrap_err()
            .contains("no manifest.json"));

        let empty_list = tar_with(&[("manifest.json", b"[]")]);
        assert!(parse_docker_save(&empty_list)
            .unwrap_err()
            .contains("no images"));

        let bad_json = tar_with(&[("manifest.json", b"{not json")]);
        assert!(parse_docker_save(&bad_json).unwrap_err().contains("JSON"));

        let zero_layers = tar_with(&[
            ("manifest.json", br#"[{"Config":"cfg.json","Layers":[]}]"#),
            ("cfg.json", CONFIG),
        ]);
        assert!(parse_docker_save(&zero_layers)
            .unwrap_err()
            .contains("no layers"));

        let missing_layer = tar_with(&[("manifest.json", MANIFEST), ("cfg.json", CONFIG)]);
        assert!(parse_docker_save(&missing_layer)
            .unwrap_err()
            .contains("missing layer layer/layer.tar"));
    }

    #[test]
    fn reads_archives_from_disk() {
        let dir = std::env::temp_dir().join(format!(
            "init-pro-registry-tarimg-{}-{:x}",
            std::process::id(),
            crate::now_millis()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("image.tar.gz");
        std::fs::write(&path, synth(true)).unwrap();
        let save = parse_docker_save_file(&path).unwrap();
        assert_eq!(save.layers, vec![LAYER.to_vec()]);
        let _ = std::fs::remove_dir_all(&dir);

        let missing = parse_docker_save_file(Path::new("/nonexistent/docker-save.tar"));
        assert!(missing.unwrap_err().contains("read"));
    }
}
