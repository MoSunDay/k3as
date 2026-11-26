//! Node-side per-attempt provider broker (TODO **T7.7**, decision **Q63**).
//!
//! Q63: sandboxed Agent/opencoder/tool/subagent environments receive NO
//! provider credentials; the ONLY provider path is this node/operator owned
//! broker — ONE Unix socket per attempt, revoked when the attempt ends. It
//! runs ONLY allowlisted operations against ONE loopback upstream URL (not a
//! generic proxy) and injects credentials upstream-only (never across the
//! sandbox-facing socket); audit records carry no secrets. Wire protocol
//! (one JSON object per connection, then EOF): request `{"op":…,"payload":…}`
//! -> `{"ok":true,"body":…}` or `{"ok":false,"error":"<reason>"}`.

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use nix::libc;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UnixListener, UnixStream};

/// 1 MiB per-message cap (request and upstream response alike).
const MAX_BYTES: u64 = 1024 * 1024;
/// Upstream write+read budget.
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(10);
/// Upstream connect budget (C7: a wedged loopback accept queue must not hold
/// the single accept loop for the whole exchange budget).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
/// Request-body read budget per connection (C7): connections are served
/// inline from the single accept loop, so a runner that never half-closes
/// is closed after this instead of wedging every later provider call.
const CONN_READ_TIMEOUT: Duration = EXCHANGE_TIMEOUT;
/// Reply write budget per connection (C7: a client that never drains its
/// reply is closed after this; the drop closes the socket).
const CONN_WRITE_TIMEOUT: Duration = Duration::from_secs(5);
/// Backoff after a transient accept error (C7: EMFILE/ECONNABORTED must not
/// hot-spin the loop).
const ACCEPT_RETRY_DELAY: Duration = Duration::from_millis(50);
/// Fixed rejection reason for non-loopback upstreams (Q63: no arbitrary egress).
const LOOPBACK_MSG: &str =
    "upstream must be a loopback http URL (Q63: broker is not a generic proxy)";

/// Operator-owned broker configuration. `upstream_url` is the ONLY egress
/// target (loopback `http://` only); credentials never cross the socket.
#[derive(Debug, Clone)]
pub struct BrokerConfig {
    pub upstream_url: String,
    pub credential_header: String,
    pub credential_value: String,
    pub allowed_operations: Vec<String>,
}

/// Deterministic broker-side decision (pure, testable without IO).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrokerDecision {
    Allow,
    Deny(String),
}

/// Allow iff `op` is on the operator allowlist (deny reason names the op).
pub fn check_operation(cfg: &BrokerConfig, op: &str) -> BrokerDecision {
    if !cfg.allowed_operations.iter().any(|a| a == op) {
        return BrokerDecision::Deny(format!("operation not allowlisted: {op}"));
    }
    BrokerDecision::Allow
}

/// Audit record for one brokered operation; NO credential material by construction.
pub fn broker_audit(agent: &str, attempt: &str, op: &str, allowed: bool) -> Value {
    json!({ "type": "BrokerOperation", "agent": agent, "attempt": attempt, "op": op,
        "allowed": allowed })
}

/// A running per-attempt broker. Dropping the handle revokes the socket
/// (sender drops -> accept loop exits -> socket unlinked, Q63);
/// [`BrokerHandle::revoke`] does it immediately.
pub struct BrokerHandle {
    pub socket_path: PathBuf,
    shutdown: tokio::sync::oneshot::Sender<()>,
}

impl BrokerHandle {
    /// Revoke explicitly (idempotent; also runs on Drop via sender-drop).
    pub async fn revoke(self) {
        let _ = self.shutdown.send(());
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

/// alnum/`-`/`_`/`.` only — header-name/path-token guard (no CRLF or path
/// separators from operator config).
fn safe_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')
}
fn safe_token(s: &str) -> bool {
    !s.is_empty() && s.chars().all(safe_char)
}

/// Attempt-id → socket filename component (engine `convert::sanitize` rules).
fn sanitize_attempt(attempt: &str) -> String {
    let out: String = attempt
        .chars()
        .take(48)
        .map(|c| if safe_char(c) { c } else { '-' })
        .collect();
    if out.is_empty() {
        "attempt".to_string()
    } else {
        out
    }
}

struct Upstream {
    host: String,
    port: u16,
    base: String,
}

/// Parse/validate the single egress target: `http://<loopback>[:port][/base]`.
fn parse_upstream(url: &str) -> Result<Upstream, String> {
    let bad = || LOOPBACK_MSG.to_string();
    let rest = url.strip_prefix("http://").ok_or_else(bad)?;
    let i = rest.find('/').unwrap_or(rest.len());
    let (authority, path) = (&rest[..i], &rest[i..]);
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse::<u16>().map_err(|_| bad())?),
        None => (authority.to_string(), 80),
    };
    if !(matches!(host.as_str(), "localhost" | "::1") || host.starts_with("127.")) {
        return Err(bad());
    }
    let up = Upstream {
        host,
        port,
        base: path.trim_end_matches('/').to_string(),
    };
    Ok(up)
}

/// Spawn: `<data_dir>/run/agent-broker/` (0o700), socket `<attempt>.sock`.
pub async fn spawn_broker(
    cfg: BrokerConfig,
    data_dir: &Path,
    agent: &str,
    attempt: &str,
) -> std::io::Result<BrokerHandle> {
    spawn_broker_bounded(cfg, data_dir, agent, attempt, CONN_READ_TIMEOUT).await
}

/// [`spawn_broker`] with an explicit per-connection read budget (C7); the
/// production entry point above pins it to [`CONN_READ_TIMEOUT`].
async fn spawn_broker_bounded(
    cfg: BrokerConfig,
    data_dir: &Path,
    agent: &str,
    attempt: &str,
    read_budget: Duration,
) -> std::io::Result<BrokerHandle> {
    let invalid = |msg: String| std::io::Error::new(std::io::ErrorKind::InvalidInput, msg);
    let up = parse_upstream(&cfg.upstream_url).map_err(invalid)?;
    if !safe_token(&cfg.credential_header) {
        return Err(invalid("credential header is not a safe token".to_string()));
    }
    if let Some(op) = cfg.allowed_operations.iter().find(|op| !safe_token(op)) {
        let msg = format!("allowed operation is not a safe path token: {op}");
        return Err(invalid(msg));
    }
    let dir = data_dir.join("run").join("agent-broker");
    std::fs::create_dir_all(&dir)?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    let socket_path = dir.join(format!("{}.sock", sanitize_attempt(attempt)));
    let _ = std::fs::remove_file(&socket_path); // stale socket from a crashed attempt
    let listener = UnixListener::bind(&socket_path)?;
    let (shutdown, rx) = tokio::sync::oneshot::channel::<()>();
    let handle = BrokerHandle {
        socket_path: socket_path.clone(),
        shutdown,
    };
    let (agent, attempt) = (agent.to_string(), attempt.to_string());
    tokio::spawn(async move {
        let mut rx = rx;
        loop {
            tokio::select! {
                _ = &mut rx => break,
                accepted = listener.accept() => match accepted {
                    Ok((conn, _)) => {
                        serve_conn(conn, &cfg, &up, &agent, &attempt, read_budget).await
                    }
                    // C7: only a listener that can never accept again ends
                    // the loop; transient errors are retried after a backoff
                    // instead of killing the broker for the rest of the attempt.
                    Err(e) if accept_error_is_fatal(&e) => {
                        tracing::debug!(target: "init-pro", error = %e,
                            "broker accept loop ended (C7)");
                        break;
                    }
                    Err(e) => {
                        tracing::debug!(target: "init-pro", error = %e,
                            "broker transient accept error (C7); retrying");
                        tokio::time::sleep(ACCEPT_RETRY_DELAY).await;
                    }
                },
            }
        }
        drop(listener);
        let _ = std::fs::remove_file(&socket_path);
    });
    Ok(handle)
}

/// C7: should the accept loop END on this error? Fatal means the listener
/// can never accept again (closed/invalid descriptor, not a socket).
/// Transient conditions -- the connection vanished before accept
/// (ECONNABORTED), fd/memory exhaustion (EMFILE/ENFILE/ENOMEM), a firewall
/// EPERM, EPROTO, EINTR/EAGAIN -- are retried. Unknown errnos and synthetic
/// errors stay fatal (the pre-C7 behavior) so a persistent programming error
/// cannot hot-spin the loop.
fn accept_error_is_fatal(err: &std::io::Error) -> bool {
    match err.raw_os_error() {
        Some(errno) => !matches!(
            errno,
            libc::EAGAIN
                | libc::EINTR
                | libc::ECONNABORTED
                | libc::EMFILE
                | libc::ENFILE
                | libc::ENOMEM
                | libc::EPERM
                | libc::EPROTO
        ),
        None => !matches!(
            err.kind(),
            std::io::ErrorKind::Interrupted
                | std::io::ErrorKind::ConnectionAborted
                | std::io::ErrorKind::WouldBlock
                | std::io::ErrorKind::TimedOut
        ),
    }
}

/// One connection: read one request object (to EOF), decide, reply once.
/// Both halves are time-bounded (C7): this runs inline in the single accept
/// loop, so a runner that never half-closes (or never drains its reply) is
/// closed after `read_budget` rather than wedging every later provider call.
async fn serve_conn(
    mut conn: UnixStream,
    cfg: &BrokerConfig,
    up: &Upstream,
    agent: &str,
    attempt: &str,
    read_budget: Duration,
) {
    let mut buf = Vec::new();
    let resp = match tokio::time::timeout(
        read_budget,
        (&mut conn).take(MAX_BYTES).read_to_end(&mut buf),
    )
    .await
    {
        // A read error (peer reset) keeps the pre-C7 shape: parse what we got.
        Ok(read) => {
            if let Err(e) = read {
                tracing::debug!(target: "init-pro", error = %e, "broker request read failed");
            }
            reply_for(&buf, cfg, up, agent, attempt).await
        }
        Err(_) => {
            tracing::warn!(target: "init-pro", agent, attempt, seconds = read_budget.as_secs(),
                "broker request read timed out (C7); closing the connection");
            json!({ "ok": false, "error": "request read timeout" })
        }
    };
    let _ = tokio::time::timeout(CONN_WRITE_TIMEOUT, async {
        conn.write_all(resp.to_string().as_bytes()).await?;
        conn.shutdown().await
    })
    .await;
}

/// Wire request `{"op":..., "payload":...}` (payload defaults to `{}`), or `None`
/// when malformed.
fn parse_request(buf: &[u8]) -> Option<(String, Value)> {
    let v = serde_json::from_slice::<Value>(buf).ok()?;
    let op = v.get("op")?.as_str()?.to_string();
    Some((op, v.get("payload").cloned().unwrap_or_else(|| json!({}))))
}

/// Decide one request and run the (single) allowlisted upstream call.
async fn reply_for(
    buf: &[u8],
    cfg: &BrokerConfig,
    up: &Upstream,
    agent: &str,
    attempt: &str,
) -> Value {
    match parse_request(buf) {
        Some((op, payload)) => match check_operation(cfg, &op) {
            BrokerDecision::Allow => {
                log_operation(agent, attempt, &op, true);
                match upstream_call(up, cfg, &op, &payload).await {
                    Ok(body) => json!({ "ok": true, "body": body }),
                    Err(e) => json!({ "ok": false, "error": e }),
                }
            }
            BrokerDecision::Deny(reason) => {
                log_operation(agent, attempt, &op, false);
                json!({ "ok": false, "error": reason })
            }
        },
        None => json!({ "ok": false, "error": "malformed request" }),
    }
}

/// Audit emission (the record itself is secret-free by construction).
fn log_operation(agent: &str, attempt: &str, op: &str, allowed: bool) {
    let rec = broker_audit(agent, attempt, op, allowed);
    if allowed {
        tracing::info!(target: "init-pro", record = %rec, "broker operation allowed");
    } else {
        tracing::warn!(target: "init-pro", record = %rec, "broker operation denied");
    }
}

/// The ONLY network the broker performs: HTTP/1.1 POST to the loopback
/// upstream (write half-closed after the POST); credentials injected here only.
async fn upstream_call(
    up: &Upstream,
    cfg: &BrokerConfig,
    op: &str,
    payload: &Value,
) -> Result<Value, String> {
    let body = payload.to_string();
    // `base` is "" or has no trailing '/', so "" also yields "/<op>".
    let path = format!("{}/{}", up.base, op);
    let req = format!(
        "POST {path} HTTP/1.1\r\nHost: {host}:{port}\r\nContent-Type: application/json\r\nContent-Length: {len}\r\n{header}: {value}\r\nX-Init-Pro-Op: {op}\r\nConnection: close\r\n\r\n{body}",
        host = up.host, port = up.port, len = body.len(), header = cfg.credential_header, value = cfg.credential_value,
    );
    // C7: bounded connect -- loopback usually fails fast, but a full backlog
    // must not hold the accept loop for the whole exchange budget.
    let mut stream = tokio::time::timeout(
        CONNECT_TIMEOUT,
        TcpStream::connect((up.host.as_str(), up.port)),
    )
    .await
    .map_err(|_| "upstream unavailable: connect timeout".to_string())?
    .map_err(|e| format!("upstream unavailable: {e}"))?;
    let raw = tokio::time::timeout(EXCHANGE_TIMEOUT, async {
        stream.write_all(req.as_bytes()).await?;
        stream.shutdown().await?;
        let mut buf = Vec::new();
        stream.take(MAX_BYTES).read_to_end(&mut buf).await?;
        Ok::<Vec<u8>, std::io::Error>(buf)
    })
    .await
    .map_err(|_| "upstream unavailable: timeout".to_string())?
    .map_err(|e| format!("upstream unavailable: {e}"))?;
    const BAD_RESP: &str = "upstream unavailable: malformed response";
    let text = String::from_utf8_lossy(&raw);
    let (head, resp_body) = text.split_once("\r\n\r\n").ok_or(BAD_RESP.to_string())?;
    let code: u16 = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse().ok())
        .ok_or(BAD_RESP.to_string())?;
    if !(200..300).contains(&code) {
        return Err(format!("upstream error: status {code}"));
    }
    let trimmed = resp_body.trim();
    Ok(serde_json::from_str(trimmed).unwrap_or_else(|_| Value::String(trimmed.to_string())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// Default fake config; "http://127.0.0.1:9" is never hit on deny paths.
    fn cfg(url: &str, ops: &[&str]) -> BrokerConfig {
        BrokerConfig {
            upstream_url: url.to_string(),
            credential_header: "Authorization".to_string(),
            credential_value: "Bearer canary-SECRET".to_string(),
            allowed_operations: ops.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// Spawn helper for the happy path (unwraps).
    async fn broker(cfg: BrokerConfig, dir: &Path, agent: &str, attempt: &str) -> BrokerHandle {
        spawn_broker(cfg, dir, agent, attempt).await.unwrap()
    }

    fn tmp(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("init-pro-broker-{tag}-{}", std::process::id()))
    }

    /// Fake local provider (AS3/AS5): records received headers, echoes hi.
    async fn spawn_fake_provider() -> (String, Arc<Mutex<Vec<(String, String)>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let rec = recorded.clone();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = listener.accept().await {
                let rec = rec.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let _ = s.read_to_end(&mut buf).await; // client uses Connection: close
                    let head = String::from_utf8_lossy(&buf);
                    let head = head.split("\r\n\r\n").next().unwrap_or("");
                    {
                        let mut hdrs = rec.lock().unwrap();
                        hdrs.extend(head.lines().skip(1).filter_map(|l| {
                            let (n, v) = l.split_once(':')?;
                            Some((n.trim().to_lowercase(), v.trim().to_string()))
                        }));
                    }
                    let resp = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 13\r\nConnection: close\r\n\r\n{\"echo\":\"hi\"}";
                    let _ = s.write_all(resp.as_bytes()).await;
                });
            }
        });
        (format!("http://127.0.0.1:{port}"), recorded)
    }

    /// One broker exchange: send `{"op":...}`, read the single JSON reply.
    async fn ask(socket: &Path, op: &str) -> Value {
        let mut s = tokio::net::UnixStream::connect(socket).await.unwrap();
        let msg = json!({ "op": op, "payload": { "m": 1 } }).to_string();
        s.write_all(msg.as_bytes()).await.unwrap();
        s.shutdown().await.unwrap();
        let mut buf = Vec::new();
        s.read_to_end(&mut buf).await.unwrap();
        serde_json::from_slice(&buf).unwrap()
    }

    #[test]
    fn check_operation_matches_allowlist() {
        let c = cfg("http://127.0.0.1:9", &["chat.completions"]);
        let allow = check_operation(&c, "chat.completions");
        assert_eq!(allow, BrokerDecision::Allow);
        let deny = check_operation(&c, "evil");
        assert!(matches!(deny, BrokerDecision::Deny(_)));
    }

    #[test]
    fn broker_audit_has_exactly_the_public_fields() {
        let a = broker_audit("ag1", "att1", "chat.completions", false);
        let mut keys: Vec<&str> = a.as_object().unwrap().keys().map(|k| k.as_str()).collect();
        keys.sort_unstable();
        assert_eq!(keys, vec!["agent", "allowed", "attempt", "op", "type"]);
        assert!(a["type"] == "BrokerOperation" && a["allowed"] == false);
    }

    #[tokio::test]
    async fn broker_allows_denies_and_injects_upstream_credentials() {
        let (upstream, recorded) = spawn_fake_provider().await;
        let dir = tmp("e2e");
        let c = cfg(&upstream, &["chat.completions"]);
        let h = broker(c, &dir, "agent-x", "att-1").await;

        let ok = ask(&h.socket_path, "chat.completions").await;
        assert!(ok["ok"] == true && ok["body"]["echo"] == "hi", "{ok}");

        let before = recorded.lock().unwrap().len();
        let denied = ask(&h.socket_path, "evil").await;
        assert_eq!(denied["ok"], false);
        assert_eq!(denied["error"], "operation not allowlisted: evil");
        assert_eq!(recorded.lock().unwrap().len(), before); // deny never hit provider

        let rec = recorded.lock().unwrap().clone();
        let auth_ok = rec
            .iter()
            .any(|(n, v)| n == "authorization" && v == "Bearer canary-SECRET");
        assert!(auth_ok);

        h.revoke().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn revoke_removes_the_socket() {
        let dir = tmp("revoke");
        let h = broker(cfg("http://127.0.0.1:9", &[]), &dir, "a", "a b/c").await;
        assert!(h.socket_path.to_string_lossy().contains("a-b-c") && h.socket_path.exists());
        let path = h.socket_path.clone();
        h.revoke().await;
        assert!(!path.exists() && tokio::net::UnixStream::connect(&path).await.is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn non_loopback_upstream_is_rejected() {
        let bad = cfg("http://10.0.0.1:80/v1", &[]);
        let err = spawn_broker(bad, &tmp("reject"), "a", "t")
            .await
            .err()
            .unwrap();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        assert!(err.to_string().contains("loopback"));
    }

    #[tokio::test]
    async fn secrets_never_cross_the_socket() {
        let dir = tmp("secrets");
        let h = broker(cfg("http://127.0.0.1:9", &[]), &dir, "agent-x", "att-2").await;
        let resp = ask(&h.socket_path, "evil").await; // deny: upstream never contacted
        let audit = broker_audit("agent-x", "att-2", "evil", false);
        let canary = "canary-SECRET";
        assert!(!resp.to_string().contains(canary) && !audit.to_string().contains(canary));
        assert!(!h.socket_path.to_string_lossy().contains(canary));
        h.revoke().await;
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)] // C7 accept-loop wedge/timeout regression cases
#[path = "agent_broker_c7_tests.rs"]
mod c7_tests;
