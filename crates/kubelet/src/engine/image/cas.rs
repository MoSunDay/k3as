//! T4.13 image store (Q53/Q54): content-addressed blob store + ref table
//! under `<data_dir>/engine/images/`.
//!
//! Layout: `blobs/<algo>/<hex>` (CAS: digest-named, deduped by rename) and
//! `refs/<refkey>.json` (the pull record: manifest digest + config/layer
//! descriptors), plus `tmp/` (in-flight downloads committed by atomic
//! rename). The M3 caches live in [`super::layers`] (`layers/` per-layer
//! unpack dirs, `views/` overlay scratch, `merged/` fused fallback) and
//! mark-sweep GC in [`super::gc`]. Pure helpers are free functions;
//! [`ImageStore`] is a cheap handle (root path + a tmp counter) safe to
//! clone.
//!
//! C11: `open` sweeps leftover `tmp/part-*` files from a previous run
//! ([`super::stage::sweep_part_files`]) and every staging site holds a
//! [`super::stage::PartGuard`], so a failed or interrupted download leaves
//! nothing behind.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::gc;
use super::manifest::Descriptor;
use super::stage::{sweep_part_files, PartGuard};

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
pub(super) fn digest_parts(digest: &str) -> (&str, &str) {
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
    /// layers/, views/, merged/, tmp/ all spring into existence. Leftover
    /// `tmp/part-*` downloads from a previous (crashed/killed) run are
    /// swept here (C11): a partial blob is never a committed blob, so
    /// dropping it is always safe.
    pub fn open(root: &Path) -> std::io::Result<Self> {
        for sub in ["blobs/sha256", "refs", "layers", "views", "merged", "tmp"] {
            std::fs::create_dir_all(root.join(sub))?;
        }
        match sweep_part_files(&root.join("tmp")) {
            Ok(0) => {}
            Ok(n) => tracing::debug!(
                target: "init-pro",
                swept = n,
                "image store: swept leftover tmp/part-* files (C11)"
            ),
            Err(e) => tracing::warn!(
                target: "init-pro",
                error = %e,
                "image store: could not sweep tmp/part-* files (C11)"
            ),
        }
        Ok(Self {
            root: root.to_path_buf(),
            tmp_seq: Arc::new(AtomicU64::new(0)),
        })
    }

    /// Stage one in-flight download: a unique `tmp/part-*` path plus its
    /// [`PartGuard`] (C11). The guard deletes the file on every exit path
    /// unless [`PartGuard::release`] hands it to [`Self::commit`].
    pub fn stage(&self) -> PartGuard {
        PartGuard::new(self.tmp_file())
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
        let guard = self.stage();
        std::fs::write(guard.path(), &bytes)
            .map_err(|e| format!("write {}: {e}", guard.path().display()))?;
        let target = self
            .root
            .join("refs")
            .join(format!("{}.json", refkey(&img.reference)));
        std::fs::rename(guard.release(), &target)
            .map_err(|e| format!("rename ref into {}: {e}", target.display()))
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

    /// T4.13 M5: on-disk usage of the durable image store — the sum of
    /// regular-file lengths under `blobs/`, `layers/` and `merged/`
    /// (one walk; `tmp/` in-flight downloads and `views/` per-container
    /// scratch are excluded — the quota is about RETAINED images).
    pub fn usage_bytes(&self) -> u64 {
        ["blobs", "layers", "merged"]
            .iter()
            .map(|sub| dir_tree_bytes(&self.root.join(sub)))
            .sum()
    }

    /// Mark-sweep GC: mark the manifest/config/layer digests of every
    /// ref, then delete CAS blobs, `merged/` fused dirs and `layers/`
    /// per-layer dirs no ref still owns (`views/` is ephemeral per-
    /// container scratch, never swept). Returns `(blobs_removed,
    /// merged_removed, layers_removed)`.
    pub fn gc(&self) -> Result<(usize, usize, usize), String> {
        gc::sweep(&self.root, &self.list_refs())
    }

    /// T4.13 M5 (the image-delete face): drop the pull record of
    /// `reference`; `true` when a ref file existed. The ref FILE is the
    /// unit of ownership — after this the next `gc()` sweeps every
    /// blob/layer/merged dir no remaining ref owns. Callers own the
    /// in-use question (a removed ref in active use frees shared layers).
    pub fn remove_ref(&self, reference: &str) -> bool {
        let path = self
            .root
            .join("refs")
            .join(format!("{}.json", refkey(reference)));
        std::fs::remove_file(&path).is_ok()
    }
}

/// Recursive sum of regular-file lengths under `dir` (0 when absent).
fn dir_tree_bytes(dir: &Path) -> u64 {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut total = 0u64;
    for entry in rd.flatten() {
        let path = entry.path();
        if path.is_dir() {
            total += dir_tree_bytes(&path);
        } else if path.is_file() {
            total += entry.metadata().map(|m| m.len()).unwrap_or(0);
        }
    }
    total
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
        for sub in ["blobs/sha256", "refs", "layers", "views", "merged", "tmp"] {
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

    /// C11: a `tmp/part-*` left by a killed run is swept at open, and only
    /// part files are (a committed blob is untouchable).
    #[test]
    fn open_sweeps_leftover_part_files() {
        let root = std::env::temp_dir().join(format!(
            "init-pro-image-cas-sweep-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        // Pre-seed what a crashed pull leaves behind: two partial blobs.
        let tmp = root.join("tmp");
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join("part-1-0-1"), b"half").unwrap();
        std::fs::write(tmp.join("part-2-7-9"), vec![0u8; 1024]).unwrap();

        let store = ImageStore::open(&root).unwrap();
        assert_eq!(
            std::fs::read_dir(&tmp).unwrap().count(),
            0,
            "all parts swept"
        );

        // Committed blobs survive a reopen (the sweep is tmp/-scoped).
        let digest = format!("sha256:{}", "e".repeat(64));
        let guard = store.stage();
        std::fs::write(guard.path(), b"payload").unwrap();
        store.commit(&guard.release(), &digest).unwrap();
        let store = ImageStore::open(&root).unwrap();
        assert!(store.has_blob(&digest), "committed blob survives the sweep");
        assert_eq!(std::fs::read_dir(&tmp).unwrap().count(), 0);
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
    fn usage_counts_durable_trees_and_excludes_tmp() {
        let (store, root) = temp_store("usage");
        assert_eq!(store.usage_bytes(), 0, "empty store");
        let img = img("reg.io/u:v1", &format!("sha256:{}", "e".repeat(64)));
        store.put_ref(&img).unwrap();
        for (digest, bytes) in [
            (img.config.digest.clone(), b"cfg-bytes".to_vec()),
            (img.layers[0].digest.clone(), vec![7u8; 512]),
        ] {
            let tmp = store.tmp_file();
            std::fs::write(&tmp, &bytes).unwrap();
            store.commit(&tmp, &digest).unwrap();
        }
        // tmp/ scratch + views/ scratch must NOT count (M5 quota face).
        std::fs::write(store.tmp_file(), vec![1u8; 4096]).unwrap();
        assert_eq!(
            store.usage_bytes(),
            (b"cfg-bytes".len() + 512) as u64,
            "blobs count, tmp does not"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn remove_ref_unowns_for_gc_and_is_idempotent() {
        let (store, root) = temp_store("rmref");
        // Distinct digests per image (the `img()` fixture shares them).
        let custom = |reference: &str, tag: char| PulledImage {
            reference: reference.to_string(),
            digest: format!("sha256:{}", tag.to_string().repeat(64)),
            config: Descriptor {
                media_type: "cfg".into(),
                digest: format!("sha256:{}", format!("{tag}c").repeat(32)),
                size: 1,
                platform: None,
            },
            layers: vec![Descriptor {
                media_type: "layer".into(),
                digest: format!("sha256:{}", format!("{tag}l").repeat(32)),
                size: 1,
                platform: None,
            }],
        };
        let gone = custom("reg.io/gone:v1", 'g');
        let keep = custom("reg.io/keep:v1", 'k');
        for img in [&gone, &keep] {
            store.put_ref(img).unwrap();
            let tmp = store.tmp_file();
            std::fs::write(&tmp, b"blob").unwrap();
            store.commit(&tmp, &img.layers[0].digest).unwrap();
        }
        assert!(store.remove_ref(&gone.reference));
        assert!(!store.remove_ref(&gone.reference), "second remove: no ref");
        assert_eq!(store.gc().unwrap(), (1, 0, 0), "only gone's blob swept");
        assert!(store.has_blob(&keep.layers[0].digest));
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

        // An orphan per-layer dir (M3): unreferenced digest is swept too.
        std::fs::create_dir_all(store.layer_dir(&orphan)).unwrap();

        let (blobs, merged, layers) = store.gc().unwrap();
        assert_eq!((blobs, merged, layers), (1, 1, 1));
        assert!(!store.has_blob(&orphan));
        assert!(!store.merged_dir(&orphan).exists());
        assert!(!store.layer_dir(&orphan).exists());
        assert!(store.has_blob(&kept.config.digest));
        assert!(store.merged_dir(&kept.digest).is_dir());
        // Idempotent: nothing left to sweep.
        assert_eq!(store.gc().unwrap(), (0, 0, 0));
        let _ = std::fs::remove_dir_all(&root);
    }
}
