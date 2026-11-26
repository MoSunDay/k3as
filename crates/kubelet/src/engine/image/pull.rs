//! T4.13 image store (Q53/Q54): the registry-v2 pull chain — single-flight
//! dedupe, manifest/index resolution with digest verification, streaming
//! CAS blob downloads, then the M3 per-layer unpack into `layers/`
//! (rootfs views are resolved lazily at container start). Reference
//! parsing lives in `reference.rs`; plain HTTP only (Q21: TLS arrives
//! with T1.3).
//!
//! C2 bounds every registry read: each wire chunk carries a per-read budget
//! (`http::BODY_READ_TIMEOUT`) and ONE pull carries an overall budget
//! ([`PULL_DEADLINE`]). A registry that accepts the connection and then
//! stalls therefore fails the pull with a "timed out" error instead of
//! parking the sync worker forever.
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
use std::time::Duration;

use sha2::Digest as _;

use super::cas::{ImageStore, PulledImage};
use super::manifest::{
    parse_manifest, select_manifest, select_platform, sha256_digest, sha256_digest_of_hasher,
    Manifest, ParsedManifest, MANIFEST_TYPES,
};
use super::reference::{host_port, normalized, parse_ref};
use crate::http::get_raw;

/// C2: overall budget for ONE image pull (manifest chain + every blob). A
/// stalled registry used to hang the sync worker indefinitely; past this
/// deadline the pull fails with a "timed out" error and the next sync cycle
/// retries. Generous on purpose -- a slow-but-flowing blob never trips it,
/// only a registry that stops delivering.
pub const PULL_DEADLINE: Duration = Duration::from_secs(10 * 60);

/// D: the most `fetch_manifest` will buffer. A manifest document is tiny
/// (KiB); the cap is what keeps an untrusted registry from sizing the
/// kubelet's heap -- `Content-Length` past it fails before a byte is read,
/// a body that grows past it while streaming fails mid-read.
pub const MAX_MANIFEST_BYTES: usize = 4 << 20;

/// The "timed out" error string for an expired pull budget (C2). The words
/// matter: the sync loop's error dedup and the e2e gates match on them.
fn pull_timed_out(what: &str) -> String {
    format!("{what}: pull timed out after {PULL_DEADLINE:?} (stalled registry?)")
}

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
    /// ref file after the lock releases). D: the entries are `Weak`, so
    /// the map is bounded by the refs IN FLIGHT, not by the distinct refs
    /// ever pulled -- once every guard is gone the entry is dead and the
    /// next `flight_lock` prunes it.
    in_flight: Arc<std::sync::Mutex<BTreeMap<String, std::sync::Weak<tokio::sync::Mutex<()>>>>>,
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
    /// so it can outlive this borrow; while it (or a waiter) lives, the
    /// weak map entry upgrades and the pull shares one lock. Dead entries
    /// (every holder gone) are pruned first (D).
    async fn flight_lock(&self, key: &str) -> tokio::sync::OwnedMutexGuard<()> {
        let lock = {
            let mut map = self
                .in_flight
                .lock()
                .expect("pull single-flight map poisoned");
            map.retain(|_, weak| weak.strong_count() > 0);
            match map.get(key).and_then(std::sync::Weak::upgrade) {
                Some(held) => held,
                None => {
                    let fresh = Arc::new(tokio::sync::Mutex::new(()));
                    map.insert(key.to_string(), Arc::downgrade(&fresh));
                    fresh
                }
            }
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
        // C2: one budget for the whole pull (manifest chain + blobs).
        let deadline = tokio::time::Instant::now() + PULL_DEADLINE;
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
            .fetch_manifest(&host, port, &repo, &target, &auth, deadline)
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
                        .fetch_manifest(&host, port, &repo, &target, &auth, deadline)
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
        self.fetch_blob(&host, port, &repo, &single.config.digest, &auth, deadline)
            .await?;
        for layer in &single.layers {
            self.fetch_blob(&host, port, &repo, &layer.digest, &auth, deadline)
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
        deadline: tokio::time::Instant,
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
        // D: an oversized Content-Length fails before a byte is read; the
        // streamed bytes are capped the same way while they land.
        if resp.content_length.unwrap_or(0) > MAX_MANIFEST_BYTES as u64 {
            return Err(format!(
                "manifest {path}: Content-Length {} exceeds the {MAX_MANIFEST_BYTES}-byte cap",
                resp.content_length.unwrap_or(0)
            ));
        }
        let bytes =
            tokio::time::timeout_at(deadline, resp.body.read_to_end_capped(MAX_MANIFEST_BYTES))
                .await
                .map_err(|_| pull_timed_out(&format!("read manifest body ({path})")))?
                .map_err(|e| format!("read manifest body ({path}): {e}"))?;
        let computed = sha256_digest(&bytes);
        if let Some(d) = reported.as_deref() {
            if d != computed {
                return Err(format!(
                    "manifest digest verification failed ({path}): header says {d}, bytes hash {computed}"
                ));
            }
        }
        // C11: the staged bytes are guarded, so a failed commit leaves no
        // `tmp/part-*` behind.
        let guard = self.store.stage();
        std::fs::write(guard.path(), &bytes)
            .map_err(|e| format!("stage manifest {}: {e}", guard.path().display()))?;
        self.store.commit(&guard.release(), &computed)?;
        Ok((bytes, computed, resp.content_type.unwrap_or_default()))
    }

    /// GET one blob, streaming to a tmp file while hashing (64 KiB chunks --
    /// the blob is NEVER buffered whole), verify against the descriptor
    /// digest, then commit into the CAS. Every read is bounded by
    /// `http::BODY_READ_TIMEOUT` and the whole body by `deadline` (C2); the
    /// staged file is owned by a `PartGuard`, so no exit path leaks it (C11).
    async fn fetch_blob(
        &self,
        host: &str,
        port: u16,
        repo: &str,
        digest: &str,
        auth: &Option<String>,
        deadline: tokio::time::Instant,
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
        // C11: `guard` owns the staged path; it deletes the file on every
        // exit path (digest mismatch, IO error, timeout, panic unwind)
        // unless `release` hands it to `commit`.
        let guard = self.store.stage();
        let mut file =
            std::fs::File::create(guard.path()).map_err(|e| format!("stage blob: {e}"))?;
        let mut hasher = sha2::Sha256::new();
        let mut buf = [0u8; 65536];
        loop {
            // C2: per-read budget inside, the pull deadline outside.
            let n = tokio::time::timeout_at(deadline, resp.body.read_chunk(&mut buf))
                .await
                .map_err(|_| pull_timed_out(&format!("download blob {digest}")))?
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
            // The guard drops here: the mismatched bytes never survive.
            return Err(format!(
                "blob digest verification failed: descriptor says {digest}, downloaded bytes hash {computed}"
            ));
        }
        self.store.commit(&guard.release(), digest)
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

    /// A one-shot fake registry (C2/C11 tests): reads the request, answers
    /// with `response`, then either closes or -- with `stall` -- holds the
    /// connection open without ever sending another byte, which is exactly
    /// the shape of a registry that hangs a sync worker.
    async fn one_shot_registry(response: Vec<u8>, stall: bool) -> (String, u16) {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                let mut req = [0u8; 1024];
                let _ = sock.read(&mut req).await;
                let _ = sock.write_all(&response).await;
                let _ = sock.flush().await;
                if stall {
                    tokio::time::sleep(Duration::from_secs(120)).await;
                }
                let _ = sock.shutdown().await;
            }
        });
        ("127.0.0.1".to_string(), addr.port())
    }

    /// C11: the staged `tmp/part-*` must be gone after any exit path.
    fn tmp_is_empty(root: &std::path::Path) -> bool {
        std::fs::read_dir(root.join("tmp"))
            .map(|rd| rd.count() == 0)
            .unwrap_or(false)
    }

    #[tokio::test]
    async fn stalled_registry_times_out_and_leaves_no_part_file() {
        let root = temp_root("stalled");
        let store = ImageStore::open(&root).unwrap();
        let puller = Puller::new("127.0.0.1:15000", store);
        // A head that promises 4 KiB, then silence forever.
        let (host, port) = one_shot_registry(
            b"HTTP/1.1 200 OK\r\nContent-Length: 4096\r\n\r\n".to_vec(),
            true,
        )
        .await;
        let digest = format!("sha256:{}", "a".repeat(64));
        let deadline = tokio::time::Instant::now() + Duration::from_millis(200);
        let started = std::time::Instant::now();
        let err = puller
            .fetch_blob(&host, port, "repo", &digest, &None, deadline)
            .await
            .expect_err("a stalled registry must fail the pull");
        assert!(err.contains("timed out"), "mapped to a timeout: {err}");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "bounded by the deadline, not by the 30s per-read budget"
        );
        assert!(
            tmp_is_empty(&root),
            "no tmp/part-* survives a timed-out download (C11)"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn digest_mismatch_leaves_no_part_file() {
        let root = temp_root("mismatch");
        let store = ImageStore::open(&root).unwrap();
        let puller = Puller::new("127.0.0.1:15000", store);
        let (host, port) = one_shot_registry(
            b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nwrong".to_vec(),
            false,
        )
        .await;
        let digest = format!("sha256:{}", "b".repeat(64));
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        let err = puller
            .fetch_blob(&host, port, "repo", &digest, &None, deadline)
            .await
            .expect_err("wrong bytes must not commit");
        assert!(err.contains("digest verification failed"), "{err}");
        assert!(tmp_is_empty(&root), "mismatched bytes are swept (C11)");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// D: a manifest body is capped -- by the announced `Content-Length`
    /// before any read, and by the streamed bytes when there is no length.
    #[tokio::test]
    async fn oversized_manifest_is_capped_in_head_and_stream() {
        let root = temp_root("big-manifest");
        let store = ImageStore::open(&root).unwrap();
        let puller = Puller::new("127.0.0.1:15000", store);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
            MAX_MANIFEST_BYTES + 1
        );
        let (host, port) = one_shot_registry(head.into_bytes(), false).await;
        let err = puller
            .fetch_manifest(&host, port, "repo", "latest", &None, deadline)
            .await
            .expect_err("an oversized manifest must not buffer");
        assert!(err.contains("exceeds"), "{err}");
        let payload = vec![b'm'; MAX_MANIFEST_BYTES + 1];
        // No Content-Length: chunked framing, so only the streamed cap
        // sees the overflow.
        let mut response = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec();
        response.extend_from_slice(format!("{:x}\r\n", payload.len()).as_bytes());
        response.extend_from_slice(&payload);
        response.extend_from_slice(b"\r\n0\r\n\r\n");
        let (host, port) = one_shot_registry(response, false).await;
        let err = puller
            .fetch_manifest(&host, port, "repo", "latest", &None, deadline)
            .await
            .expect_err("a chunked body past the cap must not buffer");
        assert!(err.contains("exceeds"), "{err}");
        assert!(tmp_is_empty(&root), "nothing was staged (C11)");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn a_good_blob_commits_and_tmp_stays_clean() {
        let root = temp_root("commit");
        let store = ImageStore::open(&root).unwrap();
        let puller = Puller::new("127.0.0.1:15000", store);
        let body = b"a perfectly good blob".to_vec();
        let digest = sha256_digest(&body);
        let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len());
        let mut response = head.into_bytes();
        response.extend_from_slice(&body);
        let (host, port) = one_shot_registry(response, false).await;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        puller
            .fetch_blob(&host, port, "repo", &digest, &None, deadline)
            .await
            .unwrap();
        assert!(puller.store.has_blob(&digest), "committed into the CAS");
        assert!(tmp_is_empty(&root), "the guard released into the rename");
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
        assert_eq!(puller.in_flight.lock().unwrap().len(), 1);
        drop(_g2);
        // D: the released ref is a dead weak now -- the NEXT lock (any key)
        // prunes it, so the map tracks what is in flight, not history.
        let g3 = puller.flight_lock("h/other:v2").await;
        {
            let map = puller.in_flight.lock().unwrap();
            assert_eq!(map.len(), 1, "the dead entry was pruned");
            assert!(!map.contains_key("h/r:v1"));
            assert!(map.contains_key("h/other:v2"));
        }
        drop(g3);
        // Single-flight itself still holds: a second taker of a HELD ref
        // waits on the same lock instead of racing a fresh one.
        let g4 = puller.flight_lock("h/s:v3").await;
        let mut waiter = Box::pin(puller.flight_lock("h/s:v3"));
        tokio::select! {
            _ = &mut waiter => panic!("a held ref must not hand out a second lock"),
            _ = tokio::time::sleep(Duration::from_millis(50)) => {}
        }
        {
            let map = puller.in_flight.lock().unwrap();
            assert_eq!(map.len(), 1, "the holder and the waiter share one entry");
            assert_eq!(map["h/s:v3"].strong_count(), 2, "holder + waiter");
        }
        drop(g4);
        let g5 = waiter.await; // handed over on release
        assert_eq!(puller.in_flight.lock().unwrap()["h/s:v3"].strong_count(), 1);
        drop(g5);
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
