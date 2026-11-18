//! T4.13 image store (Q53/Q54): content-addressed blob store + ref table +
//! fused-merge cache under `<data_dir>/engine/images/`.
//!
//! Layout: `blobs/<algo>/<hex>` (CAS: digest-named, deduped by rename),
//! `refs/<refkey>.json` (the pull record: manifest digest + config/layer
//! descriptors), `merged/<manifest-digest-hex>/` (the M2 fused rootfs
//! extracted from the layer stack), `tmp/` (in-flight downloads committed by
//! atomic rename). Pure helpers are free functions; [`ImageStore`] is a
//! cheap handle (root path + a tmp counter) safe to clone.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::manifest::Descriptor;

/// A completed pull: the normalized reference, the manifest digest (the CAS
/// identity `merged/` is keyed on), and the config/layer descriptors.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PulledImage {
    pub reference: String,
    pub digest: String,
    pub config: Descriptor,
    pub layers: Vec<Descriptor>,
}

/// `(algo, hex)` of a `algo:hex` digest; a bare hex is `sha256`-assumed.
fn digest_parts(digest: &str) -> (&str, &str) {
    match digest.split_once(':') {
        Some((algo, hex)) => (algo, hex),
        None => ("sha256", digest),
    }
}

/// Ref file key: sanitized reference (`/`, `:`, `@` -> `_`).
fn refkey(reference: &str) -> String {
    reference.replace(['/', ':', '@'], "_").replace('\\', "_")
}

/// The store handle: every operation derives paths from `root`.
#[derive(Clone)]
pub struct ImageStore {
    root: PathBuf,
    tmp_seq: Arc<AtomicU64>,
}

impl ImageStore {
    /// Open (idempotently create) the store at `root`: blobs/, refs/,
    /// merged/, tmp/ all spring into existence.
    pub fn open(root: &Path) -> std::io::Result<Self> {
        for sub in ["blobs/sha256", "refs", "merged", "tmp"] {
            std::fs::create_dir_all(root.join(sub))?;
        }
        Ok(Self {
            root: root.to_path_buf(),
            tmp_seq: Arc::new(AtomicU64::new(0)),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// CAS path of a digest: `blobs/<algo>/<hex>`.
    pub fn blob_path(&self, digest: &str) -> PathBuf {
        let (algo, hex) = digest_parts(digest);
        self.root.join("blobs").join(algo).join(hex)
    }

    pub fn has_blob(&self, digest: &str) -> bool {
        self.blob_path(digest).is_file()
    }

    /// The fused-merge rootfs dir of a manifest digest (may not exist yet).
    pub fn merged_dir(&self, digest: &str) -> PathBuf {
        let (_, hex) = digest_parts(digest);
        self.root.join("merged").join(hex)
    }

    /// A unique in-flight download path (pid + counter + nanos).
    pub fn tmp_file(&self) -> PathBuf {
        let n = self.tmp_seq.fetch_add(1, Ordering::Relaxed);
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        self.root
            .join("tmp")
            .join(format!("part-{}-{n}-{t}", std::process::id()))
    }

    /// fsync `tmp`, then atomically rename it into the CAS. If the target
    /// digest already exists the tmp is dropped and Ok returned —
    /// content-addressed dedupe (Q53).
    pub fn commit(&self, tmp: &Path, digest: &str) -> Result<(), String> {
        let target = self.blob_path(digest);
        if target.exists() {
            let _ = std::fs::remove_file(tmp);
            return Ok(());
        }
        let sync = |p: &Path| -> std::io::Result<()> { std::fs::File::open(p)?.sync_all() };
        sync(tmp).map_err(|e| format!("fsync {}: {e}", tmp.display()))?;
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
        }
        std::fs::rename(tmp, &target)
            .map_err(|e| format!("commit {} -> {}: {e}", tmp.display(), target.display()))
    }

    /// Persist a pull record atomically (tmp + rename under refs/).
    pub fn put_ref(&self, img: &PulledImage) -> Result<(), String> {
        let bytes = serde_json::to_vec(img).map_err(|e| format!("serialize ref: {e}"))?;
        let tmp = self.tmp_file();
        std::fs::write(&tmp, &bytes).map_err(|e| format!("write {}: {e}", tmp.display()))?;
        let target = self
            .root
            .join("refs")
            .join(format!("{}.json", refkey(&img.reference)));
        std::fs::rename(&tmp, &target).map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            format!("rename ref into {}: {e}", target.display())
        })
    }

    /// Read back a pull record by (normalized) reference.
    pub fn get_ref(&self, reference: &str) -> Option<PulledImage> {
        let path = self
            .root
            .join("refs")
            .join(format!("{}.json", refkey(reference)));
        let bytes = std::fs::read(path).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    /// All pull records (unsorted read_dir order).
    pub fn list_refs(&self) -> Vec<PulledImage> {
        let Ok(rd) = std::fs::read_dir(self.root.join("refs")) else {
            return Vec::new();
        };
        rd.flatten()
            .filter_map(|e| {
                let bytes = std::fs::read(e.path()).ok()?;
                serde_json::from_slice(&bytes).ok()
            })
            .collect()
    }

    /// Find the pull record whose MANIFEST digest matches (for
    /// `repo@sha256:...` references the tag spelling does not matter).
    pub fn find_ref_by_digest(&self, repo_digest: &str) -> Option<PulledImage> {
        self.list_refs()
            .into_iter()
            .find(|img| img.digest == repo_digest)
    }

    /// Mark-sweep GC: mark the manifest/config/layer digests of every ref,
    /// then delete CAS blobs and `merged/` dirs that no ref still owns.
    /// Only ever walks inside the store root. Returns
    /// `(blobs_removed, merged_removed)`.
    pub fn gc(&self) -> Result<(usize, usize), String> {
        let mut marked: Vec<String> = Vec::new();
        for img in self.list_refs() {
            marked.push(img.digest.clone());
            marked.push(img.config.digest.clone());
            marked.extend(img.layers.iter().map(|l| l.digest.clone()));
        }
        let is_marked = |d: &str| marked.iter().any(|m| m == d);

        let mut blobs_removed = 0usize;
        let blobs_root = self.root.join("blobs");
        for algo in list_dirs(&blobs_root) {
            for hex in list_files(&blobs_root.join(&algo)) {
                let digest = format!("{algo}:{hex}");
                if !is_marked(&digest) {
                    let p = blobs_root.join(&algo).join(&hex);
                    std::fs::remove_file(&p)
                        .map_err(|e| format!("gc blob {}: {e}", p.display()))?;
                    blobs_removed += 1;
                }
            }
        }

        let mut merged_removed = 0usize;
        for hex in list_dirs(&self.root.join("merged")) {
            let digest = format!("sha256:{hex}");
            if !is_marked(&digest) {
                let p = self.root.join("merged").join(&hex);
                std::fs::remove_dir_all(&p)
                    .map_err(|e| format!("gc merged {}: {e}", p.display()))?;
                merged_removed += 1;
            }
        }
        Ok((blobs_removed, merged_removed))
    }
}

/// Immediate subdirectory names (empty on any I/O error).
fn list_dirs(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.path().is_dir())
                .filter_map(|e| e.file_name().into_string().ok())
                .collect()
        })
        .unwrap_or_default()
}

/// Immediate regular-file names (empty on any I/O error).
fn list_files(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.path().is_file())
                .filter_map(|e| e.file_name().into_string().ok())
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store(tag: &str) -> (ImageStore, PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "init-pro-image-cas-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let store = ImageStore::open(&root).expect("open store");
        (store, root)
    }

    fn img(reference: &str, digest: &str) -> PulledImage {
        PulledImage {
            reference: reference.to_string(),
            digest: digest.to_string(),
            config: Descriptor {
                media_type: "application/vnd.docker.container.image.v1+json".into(),
                digest: format!("sha256:{}", "c".repeat(64)),
                size: 10,
                platform: None,
            },
            layers: vec![Descriptor {
                media_type: "application/vnd.docker.image.rootfs.diff.tar.gzip".into(),
                digest: format!("sha256:{}", "a".repeat(64)),
                size: 20,
                platform: None,
            }],
        }
    }

    #[test]
    fn open_creates_layout_and_reopen_lists_refs() {
        let (store, root) = temp_store("layout");
        for sub in ["blobs/sha256", "refs", "merged", "tmp"] {
            assert!(root.join(sub).is_dir(), "{sub} missing");
        }
        store
            .put_ref(&img(
                "127.0.0.1:15000/test/img:v1",
                &format!("sha256:{}", "d".repeat(64)),
            ))
            .unwrap();
        // Restart persistence: a fresh handle over the same root sees it.
        let reopened = ImageStore::open(&root).unwrap();
        let refs = reopened.list_refs();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].reference, "127.0.0.1:15000/test/img:v1");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn put_get_list_roundtrip_and_digest_lookup() {
        let (store, root) = temp_store("roundtrip");
        let a = img("reg.io/a/b:v1", &format!("sha256:{}", "1".repeat(64)));
        let b = img("reg.io/other:v2", &format!("sha256:{}", "2".repeat(64)));
        store.put_ref(&a).unwrap();
        store.put_ref(&b).unwrap();
        assert_eq!(store.list_refs().len(), 2);
        assert_eq!(store.get_ref("reg.io/a/b:v1").unwrap().digest, a.digest);
        assert!(store.get_ref("reg.io/missing:v9").is_none());
        assert_eq!(
            store
                .find_ref_by_digest(&format!("sha256:{}", "2".repeat(64)))
                .unwrap()
                .reference,
            "reg.io/other:v2"
        );
        assert!(store.find_ref_by_digest("sha256:nope").is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn commit_dedupes_and_gc_sweeps_unreferenced() {
        let (store, root) = temp_store("gc");
        let kept = img("reg.io/keep:v1", &format!("sha256:{}", "d".repeat(64)));
        store.put_ref(&kept).unwrap();

        // Referenced blobs (config + layer of `kept`) vs an orphan blob.
        for digest in [kept.config.digest.clone(), kept.layers[0].digest.clone()] {
            let tmp = store.tmp_file();
            std::fs::write(&tmp, b"payload").unwrap();
            assert!(!store.tmp_file().to_str().unwrap().is_empty());
            store.commit(&tmp, &digest).unwrap();
            assert!(store.has_blob(&digest));
        }
        let orphan = format!("sha256:{}", "f".repeat(64));
        let tmp = store.tmp_file();
        std::fs::write(&tmp, b"orphan").unwrap();
        store.commit(&tmp, &orphan).unwrap();

        // Dedupe: committing different bytes onto an existing digest drops
        // the tmp and keeps the original (content-addressed trust).
        let tmp2 = store.tmp_file();
        std::fs::write(&tmp2, b"other bytes").unwrap();
        store.commit(&tmp2, &kept.config.digest).unwrap();
        assert_eq!(
            std::fs::read(store.blob_path(&kept.config.digest)).unwrap(),
            b"payload"
        );
        assert!(!tmp2.exists());

        // An orphan merged dir (digest not referenced by any ref).
        std::fs::create_dir_all(store.merged_dir(&orphan)).unwrap();
        std::fs::create_dir_all(store.merged_dir(&kept.digest)).unwrap();

        let (blobs, merged) = store.gc().unwrap();
        assert_eq!((blobs, merged), (1, 1));
        assert!(!store.has_blob(&orphan));
        assert!(!store.merged_dir(&orphan).exists());
        assert!(store.has_blob(&kept.config.digest));
        assert!(store.merged_dir(&kept.digest).is_dir());
        // Idempotent: nothing left to sweep.
        assert_eq!(store.gc().unwrap(), (0, 0));
        let _ = std::fs::remove_dir_all(&root);
    }
}
