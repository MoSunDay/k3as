//! T4.13 image store (Q53/Q54): the registry-v2 pull chain — single-flight
//! dedupe, manifest/index resolution with digest verification, streaming
//! CAS blob downloads, then the M3 per-layer unpack into `layers/`
//! (rootfs views are resolved lazily at container start). Reference
//! parsing lives in `reference.rs`; plain HTTP only (Q21: TLS arrives
//! with T1.3).
//!
//! M5 adds the two operator knobs: a soft QUOTA (`with_quota`, checked
//! before any blob/layer download — a pull that would exceed the cap
//! fails cleanly, the agent survives) and env-var pull credentials
//! ([`auth_header_for`]: `INIT_PRO_PULL_AUTH_<HOST>` then
//! `INIT_PRO_PULL_AUTH`). `imagePullSecrets` stays deferred — parity with
//! the containerd path (the Sprint-20 sync.rs note: pulls were
//! unauthenticated until this env seam; the Secret-reading face is future
//! work). No credentials live in code.

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
    /// T4.13 M5: soft quota cap in bytes; 0 = unlimited (the default).
    /// Checked BEFORE any blob/layer download: `usage + incoming > quota`
    /// fails the pull cleanly so the agent loop survives (I8a).
    quota: u64,
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
            quota: 0,
            in_flight: Arc::new(std::sync::Mutex::new(BTreeMap::new())),
        }
    }

    /// T4.13 M5: builder-style soft quota (bytes, 0 = off). The cap gates
    /// blob/layer downloads only — manifests are tiny and always fetched.
    pub fn with_quota(mut self, bytes: u64) -> Self {
        self.quota = bytes;
        self
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
        // T4.13 M5: env-var credentials (INIT_PRO_PULL_AUTH_<HOST>, then
        // the bare INIT_PRO_PULL_AUTH) ride every request of this pull.
        let auth = auth_header_for(&pr.host);
        let repo = pr.repo.clone();
        let mut target = pr
            .digest
            .clone()
            .or_else(|| pr.tag.clone())
            .unwrap_or_default();
        let mut current = self
            .fetch_manifest(&host, port, &repo, &target, &auth)
            .await?;
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
                    current = self
                        .fetch_manifest(&host, port, &repo, &target, &auth)
                        .await?;
                }
                Err(e) => return Err(format!("manifest for {image:?}: {e}")),
            }
        }
        let digest = current.1;
        // T4.13 M5: the soft quota gates the EXPENSIVE part (blob/layer
        // downloads); the manifest round-trip above is tiny by design.
        // `incoming` counts only descriptors NOT already in the CAS, so a
        // re-pull of a fully-present image never trips the cap. Soft by
        // design (Q53): single-flight is per-image, so two DIFFERENT
        // images pulled concurrently can each pass the pre-check and
        // together exceed the cap — revisit if parallel image sync ever
        // lands (Sprint 43 review follow-up note).
        if self.quota > 0 {
            let incoming: u64 = std::iter::once(&single.config)
                .chain(single.layers.iter())
                .filter(|d| !self.store.has_blob(&d.digest))
                .map(|d| d.size)
                .sum();
            let using = self.store.usage_bytes();
            if using + incoming > self.quota {
                return Err(format!(
                    "builtin image store quota exceeded: using {using} bytes, pull needs +{incoming} (cap {})",
                    self.quota
                ));
            }
        }
        self.fetch_blob(&host, port, &repo, &single.config.digest, &auth)
            .await?;
        for layer in &single.layers {
            self.fetch_blob(&host, port, &repo, &layer.digest, &auth)
                .await?;
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
        auth: &Option<String>,
    ) -> Result<(Vec<u8>, String, String), String> {
        let path = format!("/v2/{repo}/manifests/{target}");
        let mut resp = get_raw(
            host,
            port,
            &path,
            &MANIFEST_TYPES.join(", "),
            &auth_slices(auth),
        )
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
        auth: &Option<String>,
    ) -> Result<(), String> {
        if self.store.has_blob(digest) {
            return Ok(());
        }
        let path = format!("/v2/{repo}/blobs/{digest}");
        let mut resp = get_raw(
            host,
            port,
            &path,
            "application/octet-stream",
            &auth_slices(auth),
        )
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

/// T4.13 M5: env-var pull credentials — the ONLY auth seam v1 has
/// (`imagePullSecrets` is deferred, mirroring the containerd path).
/// Lookup order: `INIT_PRO_PULL_AUTH_<MUNGED>` (host uppercased, every
/// non-`[A-Z0-9]` rune -> `_`, e.g. `127.0.0.1:15000` ->
/// `INIT_PRO_PULL_AUTH_127_0_0_1_15000`), then the bare
/// `INIT_PRO_PULL_AUTH`. The value must ALREADY be a ready `Authorization`
/// header value — `Basic <base64(user:pass)>` exactly as provided (the
/// caller supplies the pre-encoded secret; nothing is encoded or logged
/// here, and no credentials live in code). An unset or EMPTY value means
/// "no credentials".
pub fn auth_header_for(host: &str) -> Option<String> {
    let munged: String = host
        .to_ascii_uppercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    [
        format!("INIT_PRO_PULL_AUTH_{munged}"),
        "INIT_PRO_PULL_AUTH".to_string(),
    ]
    .into_iter()
    .filter_map(|k| std::env::var(k).ok())
    .find(|v| !v.is_empty())
}

/// `Option<auth>` -> extra-header slices for [`get_raw`] (empty = none).
fn auth_slices(auth: &Option<String>) -> Vec<(&'static str, String)> {
    match auth {
        Some(v) => vec![("Authorization", v.clone())],
        None => Vec::new(),
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

    /// ONE test fn, both cases sequential: `std::env` is process-global,
    /// so parallel test fns racing set_var/remove_var would flake (the
    /// existing env-test doctrine in `crates/cli/tests/server_env_flags.rs`).
    #[test]
    fn auth_header_for_env_order_and_munging() {
        std::env::remove_var("INIT_PRO_PULL_AUTH");
        std::env::remove_var("INIT_PRO_PULL_AUTH_127_0_0_1_15000");
        std::env::remove_var("INIT_PRO_PULL_AUTH_REG_IO");
        // Absent everywhere -> no credentials.
        assert_eq!(auth_header_for("127.0.0.1:15000"), None);
        // Bare fallback: ready-made header value passed through verbatim.
        std::env::set_var("INIT_PRO_PULL_AUTH", "Basic Zm9vOmJhcg==");
        assert_eq!(auth_header_for("reg.io"), Some("Basic Zm9vOmJhcg==".into()));
        // Host-specific wins over the bare fallback, munging ':'/'.'/'-'.
        std::env::set_var("INIT_PRO_PULL_AUTH_127_0_0_1_15000", "Basic c3BlY2lmaWM=");
        assert_eq!(
            auth_header_for("127.0.0.1:15000"),
            Some("Basic c3BlY2lmaWM=".into())
        );
        // Empty value == absent (falls through / yields None).
        std::env::set_var("INIT_PRO_PULL_AUTH_REG_IO", "");
        assert_eq!(auth_header_for("reg.io"), Some("Basic Zm9vOmJhcg==".into()));
        std::env::remove_var("INIT_PRO_PULL_AUTH");
        assert_eq!(auth_header_for("reg.io"), None);
        std::env::remove_var("INIT_PRO_PULL_AUTH_127_0_0_1_15000");
        std::env::remove_var("INIT_PRO_PULL_AUTH_REG_IO");
    }
}
