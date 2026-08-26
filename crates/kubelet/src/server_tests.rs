//! Pure-helper cases for [`crate::server`] (T4.2 Scope C, Q31). Split out of
//! server.rs to keep it under the 400-line budget; mirrors the kubectl crate
//! convention. All instant/deterministic: no sockets, no spawning.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

use runtime::cri_json::LABEL_POD_UID;

use crate::cri_backend::ContainerView;
use crate::server::{
    parse_exec_command, parse_tail_lines, resolve_container, resolve_log_path, status_json,
    tail_lines,
};

/// Unique temp sub-dir per filesystem-touching test run.
static ATOMIC_DIR: AtomicU64 = AtomicU64::new(0);

fn container(name: &str, pod_uid: &str) -> ContainerView {
    ContainerView {
        id: format!("cid-{name}"),
        sandbox_id: "sb1".to_string(),
        state: "CONTAINER_RUNNING".to_string(),
        name: name.to_string(),
        attempt: 0,
        labels: BTreeMap::from([(LABEL_POD_UID.to_string(), pod_uid.to_string())]),
        exit_code: 0,
    }
}

#[test]
fn resolve_container_filters_by_pod_uid_label() {
    let views = vec![container("a", "u1"), container("b", "u2")];
    assert_eq!(resolve_container(&views, "u2", "b").unwrap().id, "cid-b");
    // Exact-name match whose label points at another pod must NOT match.
    assert!(resolve_container(&views, "u1", "b").is_none());
}

#[test]
fn resolve_container_auto_pick_dash_takes_lexicographically_smallest() {
    let views = vec![
        container("web", "u1"),
        container("sidecar", "u1"),
        container("zz", "u9"),
    ];
    let picked = resolve_container(&views, "u1", "-").expect("auto-pick");
    assert_eq!(picked.name, "sidecar", "- picks smallest matching name");
    // No matching pod -> none.
    assert!(resolve_container(&views, "nope", "-").is_none());
}

#[test]
fn log_layout_picks_highest_attempt_ignoring_foreign_files() {
    let dir = std::env::temp_dir().join(format!(
        "kubelet-srv-tests-{}-{}",
        std::process::id(),
        ATOMIC_DIR.fetch_add(1, Ordering::SeqCst)
    ));
    let pod_dir = dir.join("pod-u1");
    std::fs::create_dir_all(&pod_dir).unwrap();
    for name in [
        "c.0.log",
        "c.3.log",
        "c.7.log",
        "other.9.log",
        "notes.txt",
        "c.broken.log",
        "c..log",
    ] {
        std::fs::write(pod_dir.join(name), b"x").unwrap();
    }
    let path = resolve_log_path(&dir, "pod-u1", "c").expect("highest attempt log exists");
    assert_eq!(
        path.file_name().unwrap().to_string_lossy(),
        "c.7.log",
        "max attempt wins; foreign files ignored"
    );
    // Missing container / missing pod dir both yield None.
    assert!(resolve_log_path(&dir, "pod-u1", "ghost").is_none());
    assert!(resolve_log_path(&dir, "pod-absent", "c").is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn tail_lines_keeps_last_n_complete_lines() {
    let all = b"l1\nl2\nl3\nl4\n";
    assert_eq!(tail_lines(all, 2).as_ref(), b"l3\nl4\n");
    // Trailing partial line counts as one line.
    let partial = b"l1\nl2\nl3";
    assert_eq!(tail_lines(partial, 1).as_ref(), b"l3");
    assert_eq!(tail_lines(partial, 2).as_ref(), b"l2\nl3");
    // n >= total lines returns the whole input unchanged (borrowed).
    assert_eq!(tail_lines(all, 4).as_ref(), &all[..]);
    assert_eq!(tail_lines(all, 99).as_ref(), &all[..]);
    // Degenerate inputs.
    assert!(tail_lines(b"", 5).is_empty());
    assert!(tail_lines(b"\n", 1).as_ref() == b"\n");
}

#[test]
fn parse_tail_lines_rejects_zero_negative_and_garbage() {
    assert_eq!(parse_tail_lines(None).unwrap(), None);
    assert_eq!(parse_tail_lines(Some("")).unwrap(), None);
    assert_eq!(parse_tail_lines(Some("tailLines=")).unwrap(), None);
    assert_eq!(parse_tail_lines(Some("tailLines=100")).unwrap(), Some(100));
    assert_eq!(parse_tail_lines(Some("a=b&tailLines=7")).unwrap(), Some(7));
    for bad in ["0", "-1", "abc", "3.5", "0x10"] {
        assert!(
            parse_tail_lines(Some(&format!("tailLines={bad}"))).is_err(),
            "{bad}"
        );
    }
    // Unknown params are ignored entirely.
    assert_eq!(parse_tail_lines(Some("follow=1")).unwrap(), None);
}

#[test]
fn parse_exec_command_shape_contract() {
    let ok = parse_exec_command(br#"{"command":["/bin/wfstep","hello"]}"#).unwrap();
    assert_eq!(ok, vec!["/bin/wfstep".to_string(), "hello".to_string()]);
    // Missing / unparseable / empty / non-array / non-string -> Err (400).
    assert!(parse_exec_command(b"").is_err());
    assert!(parse_exec_command(b"{not json").is_err());
    assert!(parse_exec_command(br#"{}"#).is_err());
    assert!(parse_exec_command(br#"{"command":[]}"#).is_err());
    assert!(parse_exec_command(br#"{"command":"/bin/sh"}"#).is_err());
    assert!(parse_exec_command(br#"{"command":["x",42]}"#).is_err());
}

#[test]
fn status_json_envelope_is_exact() {
    let v = status_json(404, "gone");
    assert_eq!(v["kind"], "Status");
    assert_eq!(v["apiVersion"], "v1");
    assert_eq!(v["status"], "Failure");
    assert_eq!(v["message"], "gone");
    assert_eq!(v["code"], 404);
}

// ---- loopback wire-contract cases (routes L + X over a real socket) -------

use std::sync::atomic::AtomicU64 as AtomicDir;
use std::sync::Arc;

use crate::cri_backend::{CriBackend, SandboxView};
use crate::runner::KubeletConfig;
use crate::server::{router, ServerState};

struct FixedContainers(Vec<ContainerView>);

#[async_trait::async_trait]
impl CriBackend for FixedContainers {
    async fn list_image_tags(&self) -> Result<Vec<String>, String> {
        Ok(vec![])
    }
    async fn pull_image(&self, _image: &str) -> Result<(), String> {
        Err("unused".to_string())
    }
    async fn list_sandboxes(&self) -> Result<Vec<SandboxView>, String> {
        Ok(vec![])
    }
    async fn list_containers(&self) -> Result<Vec<ContainerView>, String> {
        Ok(self.0.clone())
    }
    async fn run_pod_sandbox(&self, _cfg: &runtime::PodSandboxConfig) -> Result<String, String> {
        Err("unused".to_string())
    }
    async fn create_container(
        &self,
        _pod: &str,
        _c: &runtime::ContainerConfig,
        _p: &runtime::PodSandboxConfig,
    ) -> Result<String, String> {
        Err("unused".to_string())
    }
    async fn start_container(&self, _id: &str) -> Result<(), String> {
        Err("unused".to_string())
    }
    async fn stop_container(&self, _id: &str) -> Result<(), String> {
        Err("unused".to_string())
    }
    async fn remove_container(&self, _id: &str) -> Result<(), String> {
        Err("unused".to_string())
    }
    async fn stop_pod_sandbox(&self, _id: &str) -> Result<(), String> {
        Err("unused".to_string())
    }
    async fn remove_pod_sandbox(&self, _id: &str) -> Result<(), String> {
        Err("unused".to_string())
    }
}

static LOOP_DIR: AtomicDir = AtomicDir::new(0);

/// Mount [`crate::server::router`] on an ephemeral loopback port.
async fn serve_on(backend: Arc<dyn CriBackend>) -> (u16, std::path::PathBuf) {
    let data_dir = std::env::temp_dir().join(format!(
        "kubelet-srv-loop-{}-{}",
        std::process::id(),
        LOOP_DIR.fetch_add(1, Ordering::SeqCst)
    ));
    let state = ServerState {
        cfg: Arc::new(KubeletConfig::new("http://s", "n1", data_dir.clone())),
        backend,
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, router(state)).await.unwrap();
    });
    (port, data_dir)
}

fn write_log(data_dir: &std::path::Path, uid: &str, file: &str, body: &[u8]) {
    let dir = data_dir.join("pods").join(uid);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(file), body).unwrap();
}

async fn roundtrip(port: u16, req: &str) -> (u16, Vec<(String, String)>, String) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    s.write_all(req.as_bytes()).await.unwrap();
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).await.unwrap();
    let text = String::from_utf8_lossy(&raw).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((text.as_str(), ""));
    let mut lines = head.lines();
    let code = lines
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    let headers = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    (code, headers, body.to_string())
}

#[tokio::test]
async fn route_l_returns_raw_bytes_content_type_and_status_envelopes() {
    let views = vec![container("web", "u-l"), container("aux", "u-l")];
    let (port, dir) = serve_on(Arc::new(FixedContainers(views))).await;
    write_log(&dir, "u-l", "web.2.log", b"older\nnow\n");
    write_log(&dir, "u-l", "web.5.log", b"l1\nl2\nl3\n");

    // Whole file, highest attempt, exact content type per the contract.
    let (code, hdrs, body) = roundtrip(
        port,
        "GET /pods/u-l/containers/web/log HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert_eq!(code, 200);
    assert_eq!(body, "l1\nl2\nl3\n");
    let ct = hdrs.iter().find(|(k, _)| k == "content-type").unwrap();
    assert_eq!(ct.1, "text/plain; charset=utf-8");

    // tailLines=2 keeps the last two lines.
    let (code, _, body) = roundtrip(
        port,
        "GET /pods/u-l/containers/web/log?tailLines=2 HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert_eq!((code, body.as_str()), (200, "l2\nl3\n"));

    // `-` auto-picks the lexicographically smallest matching name ("aux").
    let (code, _, _) = roundtrip(
        port,
        "GET /pods/u-l/containers/-/log HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert_eq!(code, 404, "no log file for aux yet");

    // Unknown container -> 404 Status JSON; bad tailLines -> 400.
    for (path, want) in [
        ("/pods/u-l/containers/ghost/log", 404u16),
        ("/pods/u-l/containers/web/log?tailLines=0", 400),
        ("/pods/u-u9/containers/web/log?tailLines=-3", 400),
    ] {
        let (code, _, body) = roundtrip(
            port,
            &format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n"),
        )
        .await;
        assert_eq!(code, want, "{path}");
        if code != 200 {
            let v: serde_json::Value = serde_json::from_str(&body).expect("Status JSON body");
            assert_eq!(v["kind"], "Status");
            assert_eq!(v["status"], "Failure");
            assert_eq!(v["code"], want);
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn route_x_validates_body_and_reports_missing_crictl_as_502() {
    // No staged crictl in the temp data dir -> infrastructure failure.
    let views = vec![container("web", "u-x")];
    let (port, dir) = serve_on(Arc::new(FixedContainers(views))).await;

    let post = |path: &str, json: &str| {
        format!(
            "POST {path} HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{json}",
            json.len()
        )
    };

    // Bad bodies are 400 with a Status envelope (never plaintext).
    for bad in ["not json", "{\"command\":[]}", "{\"nope\":1}"] {
        let (code, _, body) = roundtrip(port, &post("/pods/u-x/containers/web/exec", bad)).await;
        assert_eq!(code, 400, "{bad}: {body}");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["kind"], "Status");
        assert_eq!(v["code"], 400);
    }

    // Unknown container is 404 even before any crictl lookup.
    let (code, _, _) = roundtrip(
        port,
        &post("/pods/u-x/containers/ghost/exec", "{\"command\":[\"ls\"]}"),
    )
    .await;
    assert_eq!(code, 404);

    // Known container + well-formed body: infra failure -> 502 Status JSON
    // (crictl binary absent in this temp data dir).
    let (code, _, body) = roundtrip(
        port,
        &post("/pods/u-x/containers/web/exec", "{\"command\":[\"true\"]}"),
    )
    .await;
    assert_eq!(code, 502);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["status"], "Failure");
    assert_eq!(v["code"], 502);
    assert!(
        v["message"]
            .as_str()
            .unwrap_or_default()
            .contains("relay target crictl exec failed"),
        "{}",
        v["message"]
    );
    let _ = std::fs::remove_dir_all(&dir);
}
