//! Shared harness for the registry V2 HTTP conformance tests (TODO
//! **T6.3**, Q40): a tiny raw HTTP/1.1 client over a real socket plus
//! the fixture builders (boot a server on an ephemeral port, push a
//! blob, push a minimal image, read tags). One shared module keeps
//! every test file under the 400-line cap; each test binary compiles
//! it separately, so a binary using only a subset must not warn.
#![allow(dead_code)]

use std::collections::HashMap;

use registry::RegistryConfig;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

// ---- tiny raw HTTP/1.1 client (Content-Length framing only) ----------------

pub struct Resp {
    pub status: u16,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

impl Resp {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(&name.to_ascii_lowercase())
            .map(|s| s.as_str())
    }
    pub fn body_str(&self) -> &str {
        std::str::from_utf8(&self.body).unwrap_or("")
    }
}

pub async fn req(
    addr: &str,
    method: &str,
    path: &str,
    extra: &[(&str, &str)],
    body: &[u8],
) -> Resp {
    // Retry the connect: loopback sockets under a busy test process can
    // transiently answer SYN with RST (stale TIME_WAIT tuple reuse).
    let mut stream = None;
    for _ in 0..50 {
        match TcpStream::connect(addr).await {
            Ok(s) => {
                stream = Some(s);
                break;
            }
            Err(_) => tokio::time::sleep(std::time::Duration::from_millis(20)).await,
        }
    }
    let mut stream = stream.unwrap_or_else(|| panic!("connect {addr}"));
    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nhost: {addr}\r\ncontent-length: {}\r\nconnection: close\r\n",
        body.len()
    );
    for (k, v) in extra {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).await.unwrap();
    stream.write_all(body).await.unwrap();
    stream.flush().await.unwrap();

    let mut raw = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = stream.read(&mut buf).await.unwrap();
        if n == 0 {
            break;
        }
        raw.extend_from_slice(&buf[..n]);
        let text_end = raw.windows(4).position(|w| w == b"\r\n\r\n");
        if let Some(hend) = text_end {
            let head = String::from_utf8_lossy(&raw[..hend]).to_ascii_lowercase();
            let clen = head
                .lines()
                .find_map(|l| {
                    l.strip_prefix("content-length:")
                        .map(|v| v.trim().parse::<usize>().ok())
                })
                .flatten()
                .unwrap_or(0);
            if raw.len() >= hend + 4 + clen {
                break;
            }
        }
    }
    parse(&raw)
}

pub fn parse(raw: &[u8]) -> Resp {
    let hend = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let head = String::from_utf8_lossy(&raw[..hend]).into_owned();
    let mut lines = head.lines();
    let status: u16 = lines
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    let mut headers = HashMap::new();
    for l in lines {
        if let Some((k, v)) = l.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    Resp {
        status,
        headers,
        body: raw[hend + 4..].to_vec(),
    }
}

// ---- fixture helpers -------------------------------------------------------

pub fn sha256_hex(b: &[u8]) -> String {
    use sha2::{Digest as _, Sha256};
    let mut h = Sha256::new();
    h.update(b);
    let out = h.finalize();
    let mut s = String::from("sha256:");
    for x in out {
        s.push_str(&format!("{x:02x}"));
    }
    s
}

/// Per-call boot sequence. Two tests in one binary can boot within the
/// same millisecond, and a pid+millis name would make them share one
/// fixture dir (the first test to finish deletes it under the other).
static BOOT_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub async fn boot(retain: usize) -> (String, std::path::PathBuf) {
    let seq = BOOT_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "init-pro-reg-it-{}-{:x}-{seq}",
        std::process::id(),
        registry::now_millis()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let root = dir.join("registry");
    std::fs::create_dir_all(&root).unwrap();
    let app = registry::v2_router(
        root.clone(),
        RegistryConfig {
            retain,
            keep_extra: vec![],
        },
    );
    tokio::spawn(async move {
        let res = axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = rx.await;
            })
            .await;
        if let Err(e) = res {
            eprintln!("SERVER TASK ERROR: {e}");
        }
    });
    // `let _ = tx` would DROP the sender now and fire the graceful
    // shutdown instantly; forget it so the server outlives boot().
    std::mem::forget(tx);
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    (addr, root)
}

/// Push a blob monolithically in one POST round-trip.
pub async fn push_blob(addr: &str, repo: &str, data: &[u8]) -> Resp {
    let d = sha256_hex(data);
    req(
        addr,
        "POST",
        &format!("/v2/{repo}/blobs/uploads/?digest={d}"),
        &[],
        data,
    )
    .await
}

/// Build + push a minimal docker v2 manifest referencing one config and
/// one layer blob. Returns (digest, tag).
pub async fn push_image(addr: &str, repo: &str, tag: &str, unique: &str) -> String {
    let cfg = format!(r#"{{"architecture":"amd64","os":"linux","unique":"{unique}"}}"#);
    let cfg_d = sha256_hex(cfg.as_bytes());
    let layer = format!("layer-of-{unique}");
    let layer_d = sha256_hex(layer.as_bytes());
    assert_eq!(push_blob(addr, repo, cfg.as_bytes()).await.status, 201);
    assert_eq!(push_blob(addr, repo, layer.as_bytes()).await.status, 201);
    let manifest = format!(
        r#"{{"schemaVersion":2,"mediaType":"application/vnd.docker.distribution.manifest.v2+json","config":{{"mediaType":"application/vnd.docker.container.image.v1+json","size":{},"digest":"{cfg_d}"}},"layers":[{{"mediaType":"application/vnd.docker.image.rootfs.diff.tar.gzip","size":{},"digest":"{layer_d}"}}]}}"#,
        cfg.len(),
        layer.len()
    );
    let r = req(
        addr,
        "PUT",
        &format!("/v2/{repo}/manifests/{tag}"),
        &[(
            "content-type",
            "application/vnd.docker.distribution.manifest.v2+json",
        )],
        manifest.as_bytes(),
    )
    .await;
    assert_eq!(r.status, 201, "{}", r.body_str());
    r.header("docker-content-digest").unwrap().to_string()
}

pub async fn tags(addr: &str, repo: &str) -> Vec<String> {
    let r = req(addr, "GET", &format!("/v2/{repo}/tags/list"), &[], b"").await;
    serde_json::from_str::<serde_json::Value>(r.body_str()).unwrap()["tags"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

pub async fn sha256_digest_of_tag(addr: &str, repo: &str, tag: &str) -> String {
    let r = req(
        addr,
        "HEAD",
        &format!("/v2/{repo}/manifests/{tag}"),
        &[],
        b"",
    )
    .await;
    r.header("docker-content-digest").unwrap().to_string()
}
