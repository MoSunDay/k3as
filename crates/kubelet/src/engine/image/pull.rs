//! T4.13 image store (Q53/Q54): the registry-v2 pull chain — single-flight
//! dedupe, manifest/index resolution with digest verification, streaming
//! CAS blob downloads, then the M3 per-layer unpack into `layers/`
//! (rootfs views are resolved lazily at container start). Reference
//! parsing lives in `reference.rs`; plain HTTP only (Q21: TLS arrives
//! with T1.3).

use std::collections::BTreeMap;
use std::io::Write as _;
use std::sync::Arc;

use sha2::Digest as _;
use tokio::io::AsyncReadExt as _;

use super::cas::{ImageStore, PulledImage};
use super::manifest::{
    parse_manifest, select_manifest, select_platform, sha256_digest, sha256_digest_of_hasher,
    Manifest, ParsedManifest, MANIFEST_TYPES,
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
        // T4.13 M3 (Q53): per-layer unpack is the pull-side artifact —
        // each layer lands ONCE in `layers/<hex>`; the fused fallback and
        // per-container overlay views resolve lazily at container start.
        let img = PulledImage {
            reference: key,
            digest,
            config: single.config,
            layers: single.layers,
        };
        self.store.ensure_layers(&img)?;
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
