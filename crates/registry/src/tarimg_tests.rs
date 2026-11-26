//! Unit tests for the `docker save` importer (T6.3/T8.1, A1 hardening).

use std::path::{Path, PathBuf};

use crate::{
    parse_docker_save, parse_docker_save_file, parse_docker_save_with, to_push_image, DockerLayer,
    ImportLimits, DEFAULT_MAX_METADATA_BYTES,
};

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
const MANIFEST: &[u8] =
    br#"[{"Config":"cfg.json","RepoTags":["example.com/demo:v1"],"Layers":["layer/layer.tar"]}]"#;

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

fn layer_bytes(layer: &DockerLayer) -> Vec<u8> {
    std::fs::read(&layer.path).unwrap()
}

fn staging_root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "init-pro-registry-tarimg-{tag}-{}-{:x}",
        std::process::id(),
        crate::now_millis()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

#[test]
fn parses_plain_and_gzip_archives() {
    for gzip in [false, true] {
        let save = parse_docker_save(&synth(gzip)).unwrap();
        assert_eq!(save.repo_tags, vec!["example.com/demo:v1"]);
        assert_eq!(save.config, CONFIG);
        assert_eq!(save.layers.len(), 1);
        assert_eq!(layer_bytes(&save.layers[0]), LAYER);
        assert_eq!(save.layers[0].size, LAYER.len() as u64);
        assert!(save.layers[0].digest.starts_with("sha256:"));
        let image = to_push_image(&save);
        assert_eq!(image.layers.len(), 1);
        assert_eq!(image.layers[0].size(), LAYER.len() as u64);
    }
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
        .contains("missing layer/layer.tar"));
}

/// A1: a layer larger than the import budget is rejected WITHOUT being
/// buffered, and the staging directory is cleaned up on the error path.
#[test]
fn oversized_archive_is_rejected_without_buffering() {
    let root = staging_root("oversized");
    let big = vec![7u8; 128 * 1024];
    let tar = tar_with(&[
        ("manifest.json", MANIFEST),
        ("cfg.json", CONFIG),
        ("layer/layer.tar", &big),
    ]);
    let limits = ImportLimits {
        max_image_bytes: 64 * 1024,
        max_metadata_bytes: DEFAULT_MAX_METADATA_BYTES,
    };
    let err = parse_docker_save_with(&tar, &root, &limits).unwrap_err();
    assert!(err.contains("import cap"), "{err}");
    // Nothing left behind.
    assert!(
        !std::fs::read_dir(&root)
            .map(|mut d| d.next().is_some())
            .unwrap_or(false),
        "staging dir must be removed on error"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// A1: peak memory is bounded by the copy window, not the image size.
#[test]
fn large_layers_are_staged_on_disk_not_in_ram() {
    let root = staging_root("large");
    // 6 MiB per layer: far beyond the 256 KiB copy window, cheap on disk.
    let a = vec![1u8; 6 * 1024 * 1024];
    let b = vec![2u8; 6 * 1024 * 1024];
    let tar = tar_with(&[
        ("manifest.json", MANIFEST),
        ("cfg.json", CONFIG),
        ("layer/layer.tar", &a),
        ("layer/extra.tar", &b),
    ]);
    let save = parse_docker_save_with(&tar, &root, &ImportLimits::default()).unwrap();
    assert_eq!(save.layers[0].size, a.len() as u64);
    assert_eq!(layer_bytes(&save.layers[0]), a);
    assert!(save.layers[0].path.is_file());
    // Drop removes the staging tree.
    let path = save.layers[0].path.clone();
    drop(save);
    assert!(!path.exists());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn reads_archives_from_disk() {
    let dir = staging_root("disk");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("image.tar.gz");
    std::fs::write(&path, synth(true)).unwrap();
    let save = parse_docker_save_file(&path).unwrap();
    assert_eq!(layer_bytes(&save.layers[0]), LAYER);
    let _ = std::fs::remove_dir_all(&dir);

    let missing = parse_docker_save_file(Path::new("/nonexistent/docker-save.tar"));
    assert!(missing.unwrap_err().contains("read"));
}
