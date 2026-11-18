//! T4.13 image store (Q53/Q54): the registry-v2 pull chain — single-flight
//! dedupe, manifest/index resolution with digest verification, streaming
//! CAS blob downloads, and the M2 fused extraction. Reference parsing lives
//! in `ref.rs`; plain HTTP only (Q21: TLS arrives with T1.3).

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;

use sha2::Digest as _;
use tokio::io::AsyncReadExt as _;

use super::cas::{ImageStore, PulledImage};
use super::manifest::{
    parse_manifest, select_manifest, select_platform, sha256_digest, sha256_digest_of_hasher,
    Descriptor, Manifest, ParsedManifest, MANIFEST_TYPES,
};
use super::reference::{host_port, normalized, parse_ref};
use crate::http::get_raw;

/// Registry pull client: one configured default registry + the CAS. Cheap
/// to construct; share one per backend so the single-flight map dedupes.
pub struct Puller {
    host: String,
    store: ImageStore,
    /// Single-flight map: concurrent pulls of the same normalized ref
    /// collapse onto one in-progress pull (the second waiter re-checks the
    /// ref file after the lock releases). Entries persist — bounded by the
    /// distinct refs ever pulled, never by transfer volume.
    in_flight: Arc<std::sync::Mutex<BTreeMap<String, Arc<tokio::sync::Mutex<()>>>>>,
}

impl Puller {
    pub fn new(registry_addr: &str, store: ImageStore) -> Self {
        Self {
            host: registry_addr.to_string(),
            store,
            in_flight: Arc::new(std::sync::Mutex::new(BTreeMap::new())),
        }
    }

    /// The configured default registry address (used when a reference
    /// carries no host of its own).
    pub fn host(&self) -> &str {
        &self.host
    }

    /// Acquire the per-ref in-flight lock. The guard is OWNED (`Arc`-based)
    /// so it can outlive this borrow — the entry stays in the map for the
    /// next pull of the same ref.
    async fn flight_lock(&self, key: &str) -> tokio::sync::OwnedMutexGuard<()> {
        let lock = {
            let mut map = self
                .in_flight
                .lock()
                .expect("pull single-flight map poisoned");
            map.entry(key.to_string()).or_default().clone()
        };
        lock.lock_owned().await
    }

    /// Pull `image` into the store and return the record. Digest-pinned
    /// refs short-circuit on a local ref hit (no network at all); tag refs
    /// ALWAYS re-fetch the manifest (`Always` policy correctness) — layers
    /// are content-addressed, so a re-pull only costs the manifest
    /// round-trip. Never panics; registry failures are `Err` strings.
    pub async fn pull(&self, image: &str) -> Result<PulledImage, String> {
        let pr = parse_ref(image, &self.host)?;
        let key = normalized(&pr);
        let _guard = self.flight_lock(&key).await;
        if let Some(d) = pr.digest.as_deref() {
            if let Some(hit) = self.store.find_ref_by_digest(d) {
                return Ok(hit);
            }
            if let Some(hit) = self.store.get_ref(&key) {
                return Ok(hit);
            }
        }
        let (host, port) = host_port(&pr.host)?;
        let repo = pr.repo.clone();
        let mut target = pr
            .digest
            .clone()
            .or_else(|| pr.tag.clone())
            .unwrap_or_default();
        let mut current = self.fetch_manifest(&host, port, &repo, &target).await?;
        let single: Manifest;
        let mut depth = 0usize;
        loop {
            match parse_manifest(&current.0) {
                Ok(ParsedManifest::Single(m)) => {
                    single = m;
                    break;
                }
                Ok(ParsedManifest::Index(idx)) => {
                    depth += 1;
                    if depth > 3 {
                        return Err(format!(
                            "manifest index nesting deeper than 3 for {image:?}"
                        ));
                    }
                    let child = select_manifest(&idx, &select_platform())
                        .map_err(|e| format!("{image:?}: {e}"))?;
                    target = child.digest;
                    current = self.fetch_manifest(&host, port, &repo, &target).await?;
                }
                Err(e) => return Err(format!("manifest for {image:?}: {e}")),
            }
        }
        let digest = current.1;
        self.fetch_blob(&host, port, &repo, &single.config.digest)
            .await?;
        for layer in &single.layers {
            self.fetch_blob(&host, port, &repo, &layer.digest).await?;
        }
        // T4.13 M3 stopgap: fused rootfs (see ensure_merged).
        self.ensure_merged(&digest, &single.layers)?;
        let img = PulledImage {
            reference: key,
            digest,
            config: single.config,
            layers: single.layers,
        };
        self.store.put_ref(&img)?;
        Ok(img)
    }

    /// GET one manifest document (small body, buffered): verify
    /// `Docker-Content-Digest` against the body hash when present, commit
    /// the bytes into the CAS. Returns `(bytes, computed_digest,
    /// content_type)`.
    async fn fetch_manifest(
        &self,
        host: &str,
        port: u16,
        repo: &str,
        target: &str,
    ) -> Result<(Vec<u8>, String, String), String> {
        let path = format!("/v2/{repo}/manifests/{target}");
        let mut resp = get_raw(host, port, &path, &MANIFEST_TYPES.join(", "))
            .await
            .map_err(|e| format!("registry {host}:{port} unreachable ({path}): {e}"))?;
        if resp.status == 404 {
            return Err(format!(
                "manifest {repo}:{target} not found in registry {host}:{port}"
            ));
        }
        if resp.status != 200 {
            return Err(format!("GET {path}: unexpected status {}", resp.status));
        }
        let reported = resp.docker_content_digest.clone();
        let bytes = resp
            .body
            .read_to_end()
            .await
            .map_err(|e| format!("read manifest body ({path}): {e}"))?;
        let computed = sha256_digest(&bytes);
        if let Some(d) = reported.as_deref() {
            if d != computed {
                return Err(format!(
                    "manifest digest verification failed ({path}): header says {d}, bytes hash {computed}"
                ));
            }
        }
        let tmp = self.store.tmp_file();
        std::fs::write(&tmp, &bytes).map_err(|e| format!("stage manifest: {e}"))?;
        self.store.commit(&tmp, &computed)?;
        Ok((bytes, computed, resp.content_type.unwrap_or_default()))
    }

    /// GET one blob, streaming to a tmp file while hashing (64 KiB chunks —
    /// the blob is NEVER buffered whole), verify against the descriptor
    /// digest, then commit into the CAS.
    async fn fetch_blob(
        &self,
        host: &str,
        port: u16,
        repo: &str,
        digest: &str,
    ) -> Result<(), String> {
        if self.store.has_blob(digest) {
            return Ok(());
        }
        let path = format!("/v2/{repo}/blobs/{digest}");
        let mut resp = get_raw(host, port, &path, "application/octet-stream")
            .await
            .map_err(|e| format!("registry {host}:{port} unreachable ({path}): {e}"))?;
        if resp.status == 404 {
            return Err(format!("blob {digest} not found in {host}:{port}/{repo}"));
        }
        if resp.status != 200 {
            return Err(format!("GET {path}: unexpected status {}", resp.status));
        }
        let tmp = self.store.tmp_file();
        let mut file = std::fs::File::create(&tmp).map_err(|e| format!("stage blob: {e}"))?;
        let mut hasher = sha2::Sha256::new();
        let mut buf = [0u8; 65536];
        loop {
            let n = resp
                .body
                .read(&mut buf)
                .await
                .map_err(|e| format!("download blob {digest}: {e}"))?;
            if n == 0 {
                break;
            }
            file.write_all(&buf[..n])
                .map_err(|e| format!("write blob bytes: {e}"))?;
            hasher.update(&buf[..n]);
        }
        file.sync_all().map_err(|e| format!("fsync blob: {e}"))?;
        drop(file);
        let computed = sha256_digest_of_hasher(hasher);
        if computed != digest {
            let _ = std::fs::remove_file(&tmp);
            return Err(format!(
                "blob digest verification failed: descriptor says {digest}, downloaded bytes hash {computed}"
            ));
        }
        self.store.commit(&tmp, digest)
    }

    /// T4.13 M3 stopgap: NAIVE fused extraction. Every layer is unpacked,
    /// IN MANIFEST ORDER, onto ONE directory — later layers overwrite
    /// earlier files. No per-layer unpack dirs, no overlayfs, no whiteout
    /// (`.wh.*`) handling, no diff_ids tracking; those land in M3. Sync
    /// filesystem work on the caller's task — fine for M2's micro images.
    pub(crate) fn ensure_merged(
        &self,
        manifest_digest: &str,
        layers: &[Descriptor],
    ) -> Result<PathBuf, String> {
        let merged = self.store.merged_dir(manifest_digest);
        if merged.is_dir() {
            return Ok(merged);
        }
        let hex = manifest_digest
            .rsplit(':')
            .next()
            .unwrap_or(manifest_digest);
        let staging = self
            .store
            .root()
            .join("merged")
            .join(format!(".tmp-{hex}-{}", std::process::id()));
        std::fs::create_dir_all(&staging).map_err(|e| format!("stage merged rootfs: {e}"))?;
        let unpack = || -> Result<(), String> {
            for layer in layers {
                // Media type decides the decompression wrap BEFORE any I/O
                // (an unsupported type fails without touching the CAS).
                let gzip = match layer.media_type.as_str() {
                    "application/vnd.docker.image.rootfs.diff.tar.gzip"
                    | "application/vnd.oci.image.layer.v1.tar+gzip"
                    | "application/vnd.oci.image.layer.nondistributable.v1.tar+gzip" => true,
                    "application/vnd.docker.image.rootfs.diff.tar"
                    | "application/vnd.oci.image.layer.v1.tar" => false,
                    other => {
                        return Err(format!(
                            "layer mediaType {other} unsupported until T4.13 M3"
                        ))
                    }
                };
                let blob = self.store.blob_path(&layer.digest);
                let file = std::fs::File::open(&blob)
                    .map_err(|e| format!("open layer {}: {e}", layer.digest))?;
                let dec: Box<dyn std::io::Read> = if gzip {
                    Box::new(flate2::read::GzDecoder::new(file))
                } else {
                    Box::new(file)
                };
                let mut arch = tar::Archive::new(dec);
                arch.set_overwrite(true);
                arch.set_unpack_xattrs(false);
                arch.set_preserve_mtime(false);
                arch.unpack(&staging)
                    .map_err(|e| format!("unpack layer {}: {e}", layer.digest))?;
            }
            Ok(())
        };
        match unpack() {
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "init-pro-image-pull-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    /// Build a deterministic gzip tar layer holding `name -> contents`.
    fn gz_layer(files: &[(&str, &str)]) -> Vec<u8> {
        let mut tar_bytes = std::io::Cursor::new(Vec::new());
        {
            let mut b = tar::Builder::new(&mut tar_bytes);
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
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(tar_bytes.get_ref()).unwrap();
        enc.finish().unwrap()
    }

    #[test]
    fn ensure_merged_unpacks_layers_in_order_later_wins() {
        let root = temp_root("merged");
        let store = ImageStore::open(&root).unwrap();
        let puller = Puller::new("127.0.0.1:15000", store.clone());
        let l1 = gz_layer(&[("bin/app", "one"), ("bin/base", "base")]);
        let l2 = gz_layer(&[("bin/app", "overwritten")]);
        let layers: Vec<Descriptor> = [l1.clone(), l2.clone()]
            .iter()
            .map(|bytes| Descriptor {
                media_type: "application/vnd.docker.image.rootfs.diff.tar.gzip".into(),
                digest: sha256_digest(bytes),
                size: bytes.len() as u64,
                platform: None,
            })
            .collect();
        for (bytes, desc) in [l1, l2].iter().zip(&layers) {
            std::fs::write(store.blob_path(&desc.digest), bytes).unwrap();
        }
        let digest = sha256_digest(b"manifest");
        let merged = puller.ensure_merged(&digest, &layers).unwrap();
        assert_eq!(
            std::fs::read_to_string(merged.join("bin/app")).unwrap(),
            "overwritten"
        );
        assert_eq!(
            std::fs::read_to_string(merged.join("bin/base")).unwrap(),
            "base"
        );
        // Cache hit: a second call returns the same dir, no new staging.
        assert_eq!(puller.ensure_merged(&digest, &layers).unwrap(), merged);
        let staged: Vec<_> = std::fs::read_dir(root.join("merged"))
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with(".tmp-"))
            .collect();
        assert!(staged.is_empty(), "staging dirs must be renamed away");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn ensure_merged_rejects_unsupported_layer_media_type() {
        let root = temp_root("unsupported");
        let store = ImageStore::open(&root).unwrap();
        let puller = Puller::new("127.0.0.1:15000", store);
        let layer = Descriptor {
            media_type: "application/vnd.oci.image.layer.v1.tar+zstd".into(),
            digest: sha256_digest(b"zstd-bytes"),
            size: 11,
            platform: None,
        };
        let err = puller.ensure_merged("sha256:m", &[layer]).unwrap_err();
        assert!(err.contains("unsupported until T4.13 M3"), "{err}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn single_flight_lock_reuses_entries() {
        let root = temp_root("flight");
        let store = ImageStore::open(&root).unwrap();
        let puller = Puller::new("127.0.0.1:15000", store);
        let g1 = puller.flight_lock("h/r:v1").await;
        assert_eq!(puller.in_flight.lock().unwrap().len(), 1);
        drop(g1);
        let _g2 = puller.flight_lock("h/r:v1").await; // reusable after release
        drop(_g2);
        let _ = std::fs::remove_dir_all(&root);
    }
}
