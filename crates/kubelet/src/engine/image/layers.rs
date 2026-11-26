//! T4.13 image store (Q53/Q54, M3): the per-layer unpack cache + the
//! per-container overlay views over it. Split from `cas.rs` by
//! responsibility (file-size rule): `layers/<hex>/` holds each layer
//! unpacked ONCE (immutable, shared, kernel whiteouts inside), `views/
//! <id>/{merged,upper,work}` is per-container overlayfs scratch, and
//! `merged/<manifest-hex>/` is the whiteout-aware fused fallback rootfs.

use std::path::{Path, PathBuf};

use super::cas::{ImageStore, PulledImage};
use super::unpack;

/// Prefix of every publish-by-rename staging dir under `layers/` and
/// `merged/`: [`ImageStore::ensure_layers`] and `ensure_merged_fused`
/// write into it and rename it onto the digest name when complete. The
/// sweeps (`gc.rs`, the disk-pressure pass) skip these names (D): a GC
/// racing an in-flight unpack must never delete the tree being written.
pub(crate) const STAGING_PREFIX: &str = ".tmp-";

/// Pure: whether a `layers/` / `merged/` entry is an in-flight staging dir.
pub(crate) fn is_staging_name(name: &str) -> bool {
    name.starts_with(STAGING_PREFIX)
}

/// The staging dir of one digest under `parent` (pid-suffixed, so two
/// agents sharing a root never stage into the same tree).
fn staging_dir(parent: &Path, hex: &str) -> PathBuf {
    parent.join(format!("{STAGING_PREFIX}{hex}-{}", std::process::id()))
}

/// A per-container overlayfs view (Q53 M3): the overlay is mounted at
/// `mountpoint` with `upper`/`work` scratch on top of the shared layer
/// dirs. Cheap plain data; paths are owned by the store root.
#[derive(Debug, Clone)]
pub struct ViewPaths {
    pub mountpoint: PathBuf,
    pub upper: PathBuf,
    pub work: PathBuf,
}

impl ImageStore {
    /// The per-layer unpack dir of a layer digest: unpacked once,
    /// immutable afterwards, shared by every container using it.
    pub fn layer_dir(&self, digest: &str) -> PathBuf {
        let (_, hex) = super::cas::digest_parts(digest);
        self.root().join("layers").join(hex)
    }

    pub fn has_layer(&self, digest: &str) -> bool {
        self.layer_dir(digest).is_dir()
    }

    /// Unpack every layer of `img` that is not already present, staging
    /// into `layers/.tmp-*` and publishing by atomic rename — a layer
    /// dir either does not exist or is complete. Idempotent.
    pub fn ensure_layers(&self, img: &PulledImage) -> Result<(), String> {
        for layer in &img.layers {
            let dir = self.layer_dir(&layer.digest);
            if dir.is_dir() {
                continue;
            }
            let hex = layer.digest.rsplit(':').next().unwrap_or(&layer.digest);
            let staging = staging_dir(&self.root().join("layers"), hex);
            match unpack::unpack_layer(&self.blob_path(&layer.digest), &staging, &layer.media_type)
            {
                Ok(()) => std::fs::rename(&staging, &dir).map_err(|e| {
                    let _ = std::fs::remove_dir_all(&staging);
                    format!("publish layer {}: {e}", layer.digest)
                })?,
                Err(e) => {
                    let _ = std::fs::remove_dir_all(&staging);
                    return Err(e);
                }
            }
        }
        Ok(())
    }

    /// Whiteout-aware fused rootfs (M3 semantics) of `img`, cached under
    /// `merged/<manifest-hex>`: exists -> return, else stage + rename.
    /// The fallback path when per-container overlayfs views are off.
    pub fn ensure_merged_fused(&self, img: &PulledImage) -> Result<PathBuf, String> {
        let merged = self.merged_dir(&img.digest);
        if merged.is_dir() {
            return Ok(merged);
        }
        let hex = img.digest.rsplit(':').next().unwrap_or(&img.digest);
        let staging = staging_dir(&self.root().join("merged"), hex);
        let blobs: Vec<PathBuf> = img
            .layers
            .iter()
            .map(|l| self.blob_path(&l.digest))
            .collect();
        let media: Vec<String> = img.layers.iter().map(|l| l.media_type.clone()).collect();
        match unpack::fused_layers(&blobs, &media, &staging) {
            Ok(()) => {
                std::fs::rename(&staging, &merged).map_err(|e| {
                    let _ = std::fs::remove_dir_all(&staging);
                    format!("publish merged rootfs: {e}")
                })?;
                Ok(merged)
            }
            Err(e) => {
                let _ = std::fs::remove_dir_all(&staging);
                Err(e)
            }
        }
    }

    /// Create the per-container overlayfs view `views/<id>/{merged,
    /// upper, work}` (Q53). A fresh id gets a fresh view.
    pub fn new_view(&self, container_id: &str) -> Result<ViewPaths, String> {
        let base = self.root().join("views").join(container_id);
        for sub in ["merged", "upper", "work"] {
            std::fs::create_dir_all(base.join(sub)).map_err(|e| format!("view dir {sub}: {e}"))?;
        }
        Ok(ViewPaths {
            mountpoint: base.join("merged"),
            upper: base.join("upper"),
            work: base.join("work"),
        })
    }

    /// Drop one container's view (best-effort; container teardown).
    pub fn remove_view(&self, container_id: &str) {
        let _ = std::fs::remove_dir_all(self.root().join("views").join(container_id));
    }

    /// Drop ALL views: an agent restart killed the engines, the scratch
    /// dirs are dead weight (Q49 recover hygiene).
    pub fn clear_views(&self) {
        if let Ok(rd) = std::fs::read_dir(self.root().join("views")) {
            for entry in rd.flatten() {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::super::manifest::Descriptor;
    use super::*;

    /// Deterministic (uncompressed) tar bytes for `name -> contents`.
    fn tar_bytes(files: &[(&str, &str)]) -> Vec<u8> {
        let mut c = std::io::Cursor::new(Vec::new());
        {
            let mut b = tar::Builder::new(&mut c);
            for (name, content) in files {
                let mut h = tar::Header::new_gnu();
                h.set_size(content.len() as u64);
                h.set_mode(0o644);
                h.set_cksum();
                b.append_data(&mut h, name, std::io::Cursor::new(content.as_bytes()))
                    .unwrap();
            }
            b.finish().unwrap();
        }
        c.into_inner()
    }

    fn temp_store(tag: &str) -> (ImageStore, PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "init-pro-image-layers-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let store = ImageStore::open(&root).expect("open store");
        (store, root)
    }

    /// A real gzip layer blob committed into the CAS.
    fn layer_blob(store: &ImageStore, digest: &str, files: &[(&str, &str)]) {
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(&tar_bytes(files)).unwrap();
        let bytes = enc.finish().unwrap();
        let tmp = store.tmp_file();
        std::fs::write(&tmp, &bytes).unwrap();
        store.commit(&tmp, digest).unwrap();
    }

    fn two_layer_img() -> (ImageStore, PathBuf, PulledImage) {
        let (store, root) = temp_store("two");
        let l1 = format!("sha256:{}", "1".repeat(64));
        let l2 = format!("sha256:{}", "2".repeat(64));
        layer_blob(&store, &l1, &[("bin/app", "one"), ("gone/old", "x")]);
        layer_blob(&store, &l2, &[("gone/.wh.old", ""), ("bin/app2", "two")]);
        let img = PulledImage {
            reference: "reg.io/lay:v1".into(),
            digest: format!("sha256:{}", "d".repeat(64)),
            config: Descriptor {
                media_type: "application/vnd.docker.container.image.v1+json".into(),
                digest: format!("sha256:{}", "c".repeat(64)),
                size: 10,
                platform: None,
            },
            layers: vec![
                Descriptor {
                    media_type: "application/vnd.docker.image.rootfs.diff.tar.gzip".into(),
                    digest: l1,
                    size: 20,
                    platform: None,
                },
                Descriptor {
                    media_type: "application/vnd.docker.image.rootfs.diff.tar.gzip".into(),
                    digest: l2,
                    size: 20,
                    platform: None,
                },
            ],
        };
        (store, root, img)
    }

    #[test]
    fn ensure_layers_is_idempotent_and_shared() {
        if !nix::unistd::Uid::effective().is_root() {
            return; // kernel whiteout translation needs root (Q53 M3)
        }
        let (store, root, img) = two_layer_img();
        store.ensure_layers(&img).unwrap();
        for layer in &img.layers {
            assert!(store.has_layer(&layer.digest), "layer dir present");
        }
        assert!(store
            .layer_dir(&img.layers[1].digest)
            .join("gone/old")
            .exists());
        // Second call is a no-op: every dir is already complete, and no
        // staging dirs are left behind.
        store.ensure_layers(&img).unwrap();
        let stray: Vec<_> = std::fs::read_dir(root.join("layers"))
            .unwrap()
            .flatten()
            .filter(|e| is_staging_name(&e.file_name().to_string_lossy()))
            .collect();
        assert!(stray.is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn fused_fallback_applies_whiteouts_and_caches() {
        let (store, root, img) = two_layer_img();
        let merged = store.ensure_merged_fused(&img).unwrap();
        assert!(!merged.join("gone/old").exists(), "fused whiteout");
        assert_eq!(
            std::fs::read_to_string(merged.join("bin/app")).unwrap(),
            "one"
        );
        assert_eq!(merged, store.ensure_merged_fused(&img).unwrap(), "cached");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn views_roundtrip_and_clear() {
        let (store, root, _img) = two_layer_img();
        let view = store.new_view("c-1").unwrap();
        assert!(view.mountpoint.is_dir() && view.upper.is_dir() && view.work.is_dir());
        store.remove_view("c-1");
        assert!(!view.mountpoint.exists());
        store.new_view("c-2").unwrap();
        store.clear_views();
        assert_eq!(std::fs::read_dir(root.join("views")).unwrap().count(), 0);
        let _ = std::fs::remove_dir_all(&root);
    }
}
