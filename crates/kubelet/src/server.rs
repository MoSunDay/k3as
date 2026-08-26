//! Kubelet local container API: **logs** + one-shot **exec** over plain HTTP
//! (TODO **T4.2 Scope C**, decision **Q31**).
//!
//! The apiserver relay dials this listener (`cfg.node_ip:cfg.kubelet_port`,
//! both advertised on the Node object) instead of shelling crictl locally.
//! Frozen wire contract:
//!
//! - Route L: `GET /pods/{pod_uid}/containers/{container}/log?tailLines=N`
//!   -> `text/plain; charset=utf-8`, body = raw highest-attempt log file
//!   (`<data_dir>/pods/<pod-uid>/<container>.<attempt>.log`, the layout
//!   [`crate::sync`] writes through `runtime::ContainerConfig::log_path`).
//! - Route X: `POST /pods/{pod_uid}/containers/{container}/exec` with body
//!   `{"command":["/bin/wfstep","hello"]}` (non-empty argv, NO shell) ->
//!   200 `{"exitCode": <i32 as i64>, "output": "<merged stdout+stderr>"}`.
//!   NONZERO container exit codes are a normal 200 (the rc rides the body);
//!   infrastructure failure (missing crictl binary, spawn error, timeout)
//!   is 502 Status JSON.
//! - Errors always use the Status-shaped envelope built by [`status_json`] -
//!   no plaintext bodies (Q10 JSON-only wire format): 404 unknown container
//!   or missing log file, 500 log read failure, 400 bad `tailLines`.
//!
//! `{container}` may be the literal `-`: AUTO-PICK the lexicographically
//! SMALLEST container name among containers whose `io.kubernetes.pod.uid`
//! label matches (v1 simplification; multi-container pods should name the
//! container explicitly).
//!
//! Style notes: axum 0.8 mirroring `apiserver::app`; state immutable after
//! startup; query + body parsing live in pure helpers ([`parse_tail_lines`],
//! [`parse_exec_command`]) so the tricky parts are unit-tested without
//! spawning and this crate keeps its no-direct-`serde` dependency shape.

use std::borrow::Cow;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
// (`std::path::Path` is spelled fully-qualified below — `Path` here is the
// axum route-param extractor.)
use axum::body::Bytes;
use axum::extract::{Path, RawQuery, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use runtime::cri_json::LABEL_POD_UID;

use crate::cri_backend::{ContainerView, CriBackend};
use crate::runner::KubeletConfig;

/// Wall-clock cap for one exec round-trip. Mirrors the crictl-side
/// `--timeout 30s` flag set by `runtime::CriCtl::exec_container`: both layers
/// carry the same bound (the explicit-flag lesson from the earlier flake).
pub(crate) const EXEC_TIMEOUT: Duration = Duration::from_secs(30);

/// Literal `{container}` token meaning "auto-pick" (see module docs).
const AUTO_PICK: &str = "-";

/// Immutable shared handler state (`pub(crate)` so the in-crate test
/// module can mount the same router on an ephemeral loopback port).
#[derive(Clone)]
pub(crate) struct ServerState {
    pub(crate) cfg: Arc<KubeletConfig>,
    pub(crate) backend: Arc<dyn CriBackend>,
}

/// Serve the local log/exec API on `cfg.node_ip:cfg.kubelet_port` until
/// `shutdown` resolves; axum drains in-flight requests afterwards (mirrors
/// `apiserver::serve`).
pub async fn serve(
    cfg: KubeletConfig,
    backend: Arc<dyn CriBackend>,
    shutdown: infra::Shutdown,
) -> std::io::Result<()> {
    let addr: SocketAddr = format!("{}:{}", cfg.node_ip, cfg.kubelet_port)
        .parse()
        .map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "kubelet: invalid node ip {} / port {}: {e}",
                    cfg.node_ip, cfg.kubelet_port
                ),
            )
        })?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(
        target: "init-pro",
        %addr, node = %cfg.node_name,
        "kubelet local API listening (container logs/exec, T4.2 Scope C)"
    );
    let app = router(ServerState {
        cfg: Arc::new(cfg),
        backend,
    });
    axum::serve(listener, app.into_make_service())
        .with_graceful_shutdown(async move { shutdown.cancelled().await })
        .await
}

pub(crate) fn router(state: ServerState) -> Router {
    Router::new()
        .route(
            "/pods/{pod_uid}/containers/{container}/log",
            get(log_handler),
        )
        .route(
            "/pods/{pod_uid}/containers/{container}/exec",
            post(exec_handler),
        )
        .with_state(state)
}

// ---- pure helpers (unit-tested without spawning; see server_tests.rs) ------

/// Status-shaped error envelope - the ONLY error body this listener emits.
pub(crate) fn status_json(code: u16, msg: &str) -> serde_json::Value {
    serde_json::json!({
        "kind": "Status",
        "apiVersion": "v1",
        "status": "Failure",
        "message": msg,
        "code": code,
    })
}

/// Resolve `{container}` within `pod_uid`: exact name match, or the `-`
/// auto-pick rule (lexicographically smallest matching container name).
pub(crate) fn resolve_container<'a>(
    views: &'a [ContainerView],
    pod_uid: &str,
    container: &str,
) -> Option<&'a ContainerView> {
    let pod_match =
        |v: &&'a ContainerView| v.labels.get(LABEL_POD_UID).map(String::as_str) == Some(pod_uid);
    if container == AUTO_PICK {
        views
            .iter()
            .filter(pod_match)
            .min_by(|a, b| a.name.cmp(&b.name))
    } else {
        views.iter().find(|v| pod_match(v) && v.name == container)
    }
}

/// `<container-name>.<attempt>.log` -> `<attempt>`; `None` for every other
/// filename shape (robust to foreign files in the pod log directory).
fn attempt_of(file_name: &str, container: &str) -> Option<u32> {
    let stem = file_name.strip_suffix(".log")?;
    let digits = stem.strip_prefix(container)?.strip_prefix('.')?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// Highest-attempt EXISTING log file for one container under
/// `<log_root>/<pod_uid>/`; `None` when the pod dir (or every candidate
/// attempt) is missing. Higher attempt numbers win: restarts bump the
/// attempt and the latest incarnation owns the freshest output.
pub(crate) fn resolve_log_path(
    log_root: &std::path::Path,
    pod_uid: &str,
    container: &str,
) -> Option<PathBuf> {
    let dir = log_root.join(pod_uid);
    let mut best: Option<(u32, PathBuf)> = None;
    for entry in std::fs::read_dir(&dir).ok()?.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if let Some(attempt) = attempt_of(&name, container) {
            if best.as_ref().is_none_or(|(hi, _)| attempt > *hi) {
                best = Some((attempt, dir.join(name.as_ref())));
            }
        }
    }
    best.map(|(_, path)| path)
}

/// Parse the `tailLines=N` query parameter from a raw query string:
/// absent/empty param = whole file (`Ok(None)`); non-numeric, `0` or negative
/// values are rejected (`Err` -> HTTP 400).
pub(crate) fn parse_tail_lines(query: Option<&str>) -> Result<Option<usize>, String> {
    let raw = query
        .unwrap_or_default()
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(k, _)| *k == "tailLines")
        .map(|(_, v)| v)
        .unwrap_or_default();
    if raw.is_empty() {
        return Ok(None);
    }
    match raw.parse::<i64>() {
        // Positive only: `0`/negative are rejected per the route contract.
        Ok(n) if n > 0 => Ok(Some(n.min(i64::from(u32::MAX)) as usize)),
        _ => Err(format!("tailLines must be a positive integer, got {raw:?}")),
    }
}

/// Keep the LAST `n` lines of `bytes` (line boundary `\n`; a trailing
/// partial line without a newline counts as one line). Borrows the input
/// slice when the whole file fits within `n` lines (zero-copy fast path).
pub(crate) fn tail_lines<'a>(bytes: &'a [u8], n: usize) -> Cow<'a, [u8]> {
    if n == 0 {
        return Cow::Borrowed(&[]);
    }
    let ends_nl = bytes.last() == Some(&b'\n');
    let partial_tail = usize::from(!bytes.is_empty() && !ends_nl);
    let total_lines = bytes.iter().filter(|&&b| b == b'\n').count() + partial_tail;
    if total_lines <= n {
        return Cow::Borrowed(bytes);
    }
    // Skip exactly (total_lines - n) leading lines; keep the remainder.
    let mut start = 0usize;
    for _ in 0..(total_lines - n) {
        match bytes[start..].iter().position(|&b| b == b'\n') {
            Some(pos) => start += pos + 1,
            None => break,
        }
    }
    Cow::Borrowed(&bytes[start..])
}

/// Validate an exec body -> argv words. Empty body, unparseable JSON,
/// missing/non-array/empty `command`, or non-string elements are all `Err`
/// (surfaced as HTTP 400 Status JSON).
pub(crate) fn parse_exec_command(body: &[u8]) -> Result<Vec<String>, String> {
    if body.is_empty() {
        return Err("missing request body (expected {\"command\": [...]})".to_string());
    }
    let value: serde_json::Value =
        serde_json::from_slice(body).map_err(|e| format!("unparseable request body: {e}"))?;
    let words = value
        .get("command")
        .ok_or_else(|| "missing `command` field".to_string())?
        .as_array()
        .ok_or_else(|| "`command` must be an array of strings".to_string())?;
    if words.is_empty() {
        return Err("`command` must be non-empty".to_string());
    }
    words
        .iter()
        .map(|w| {
            w.as_str()
                .map(str::to_string)
                .ok_or_else(|| "`command` elements must be strings".to_string())
        })
        .collect()
}

// ---- responses -------------------------------------------------------------

/// Any failure answer: Status-shaped JSON envelope with the matching code.
fn failure(code: StatusCode, msg: &str) -> Response {
    (code, Json(status_json(code.as_u16(), msg))).into_response()
}

fn log_success(bytes: Vec<u8>) -> Response {
    (
        StatusCode::OK,
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/plain; charset=utf-8"),
        )],
        bytes,
    )
        .into_response()
}

// ---- handlers --------------------------------------------------------------

/// Route L: `GET /pods/{pod_uid}/containers/{container}/log?tailLines=N`.
async fn log_handler(
    State(st): State<ServerState>,
    Path((pod_uid, container)): Path<(String, String)>,
    RawQuery(query): RawQuery,
) -> Response {
    let tail = match parse_tail_lines(query.as_deref()) {
        Ok(t) => t,
        Err(msg) => return failure(StatusCode::BAD_REQUEST, &msg),
    };
    let views = match st.backend.list_containers().await {
        Ok(v) => v,
        Err(e) => {
            return failure(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("container listing failed: {e}"),
            )
        }
    };
    let Some(view) = resolve_container(&views, &pod_uid, &container) else {
        return failure(
            StatusCode::NOT_FOUND,
            &format!("container {container:?} of pod {pod_uid:?} not found"),
        );
    };
    let Some(path) = resolve_log_path(&st.cfg.log_root(), &pod_uid, &view.name) else {
        return failure(
            StatusCode::NOT_FOUND,
            &format!(
                "no log file found for container {:?} of pod {:?}",
                view.name, pod_uid
            ),
        );
    };
    match tokio::fs::read(&path).await {
        Ok(bytes) => log_success(match tail {
            None => bytes,
            Some(n) => tail_lines(&bytes, n).into_owned(),
        }),
        Err(e) => failure(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("read log {}: {e}", path.display()),
        ),
    }
}

/// Route X: `POST /pods/{pod_uid}/containers/{container}/exec`. One-shot:
/// argv only (no shell), merged stdout+stderr, exit code in the body.
async fn exec_handler(
    State(st): State<ServerState>,
    Path((pod_uid, container)): Path<(String, String)>,
    body: Bytes,
) -> Response {
    let command = match parse_exec_command(&body) {
        Ok(cmd) => cmd,
        Err(msg) => return failure(StatusCode::BAD_REQUEST, &msg),
    };
    let views = match st.backend.list_containers().await {
        Ok(v) => v,
        Err(e) => {
            return failure(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("container listing failed: {e}"),
            )
        }
    };
    let Some(view) = resolve_container(&views, &pod_uid, &container) else {
        return failure(
            StatusCode::NOT_FOUND,
            &format!("container {container:?} of pod {pod_uid:?} not found"),
        );
    };
    // Same driver construction the runner/cli wiring uses (Q26 route B):
    // staged crictl + supervisor socket derived from the data dir.
    let paths = runtime::AgentRuntimePaths::for_data_dir(&st.cfg.data_dir);
    let Some(ctl) = runtime::CriCtl::for_paths(&paths) else {
        return failure(
            StatusCode::BAD_GATEWAY,
            "relay target crictl exec failed: no staged crictl binary (T4.1)",
        );
    };
    match ctl.exec_container(&view.id, &command, EXEC_TIMEOUT).await {
        Ok((exit_code, output)) => Json(serde_json::json!({
            "exitCode": exit_code as i64,
            "output": output,
        }))
        .into_response(),
        Err(e) => failure(
            StatusCode::BAD_GATEWAY,
            &format!("relay target crictl exec failed: {e}"),
        ),
    }
}
