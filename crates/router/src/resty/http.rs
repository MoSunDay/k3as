//! `resty.http` — HTTP/1.1 client over TCP+TLS (T5.3 Scope B).
//!
//! Parity with `lua-resty-http`'s convenience entry point `request_uri`: a
//! single-shot request that opens a TCP connection, optionally TLS-upgrades
//! (reusing [`crate::tls::build_client_config`]), sends the request, and reads
//! the full response into `{ status, headers, body }`.
//!
//! Outbound TLS shares the same `ring`-provider config builder as cosocket
//! `sslhandshake`, so `verify=false` (self-signed / internal mesh) works
//! identically on both paths (ADR **Q16**).

use std::sync::Arc;

use mlua::{Lua, LuaString, Table, Value};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

/// Build the `resty.http` table: `{ request_uri = fn }`.
pub fn build(lua: &Lua) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    t.raw_set("request_uri", lua.create_async_function(request_uri)?)?;
    Ok(t)
}

/// `resty.http.request_uri(uri, opts?)` -> `{ status, headers, body }`.
async fn request_uri(lua: Lua, (uri, opts): (String, RequestOpts)) -> mlua::Result<Table> {
    let uri = parse_uri(&uri)?;
    let req = build_request(&uri, &opts)?;
    let timeout_ms = opts.timeout_ms;

    let stream = TcpStream::connect((uri.host.as_str(), uri.port))
        .await
        .map_err(|e| err(format!("http: connect {0}:{1}: {e}", uri.host, uri.port)))?;

    let raw = if uri.tls {
        let cfg = crate::tls::build_client_config(opts.verify.unwrap_or(true))
            .map_err(|e| err(format!("http: tls config: {e}")))?;
        let connector = TlsConnector::from(Arc::new(cfg));
        let name = rustls::pki_types::ServerName::try_from(uri.host.clone())
            .map_err(|e| err(format!("http: bad host {0:?}: {e}", uri.host)))?;
        let mut tls = connect_bounded(timeout_ms, connector.connect(name, stream))
            .await
            .ok_or_else(|| err("http: tls connect timeout".into()))??;
        round_trip(&mut tls, &req).await?
    } else {
        let mut plain = stream;
        round_trip(&mut plain, &req).await?
    };

    make_response(&lua, &raw)
}

/// Send `req`, then read until EOF (we always set `Connection: close`).
async fn round_trip<S>(stream: &mut S, req: &[u8]) -> mlua::Result<Vec<u8>>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    stream
        .write_all(req)
        .await
        .map_err(|e| err(format!("http: write: {e}")))?;
    let mut buf = Vec::new();
    stream
        .read_to_end(&mut buf)
        .await
        .map_err(|e| err(format!("http: read: {e}")))?;
    Ok(buf)
}

// ---- request shaping ----

struct Uri {
    tls: bool,
    host: String,
    port: u16,
    path: String,
}

/// Minimal `scheme://host[:port]/path` parser (no userinfo, no query split —
/// the query, if any, stays in `path`, exactly as `lua-resty-http` expects).
fn parse_uri(s: &str) -> mlua::Result<Uri> {
    let (scheme, rest) = s
        .split_once("://")
        .ok_or_else(|| err("http: uri must include scheme (http:// or https://)".into()))?;
    let tls = match scheme {
        "https" => true,
        "http" => false,
        other => return Err(err(format!("http: unsupported scheme {other:?}"))),
    };
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], rest[i..].to_owned()),
        None => (rest, "/".to_owned()),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (
            h.to_owned(),
            p.parse::<u16>().map_err(|_| err(format!("http: bad port {p:?}")))?,
        ),
        None => (authority.to_owned(), if tls { 443 } else { 80 }),
    };
    Ok(Uri { tls, host, port, path })
}

fn build_request(uri: &Uri, opts: &RequestOpts) -> mlua::Result<Vec<u8>> {
    let mut head = String::new();
    head.push_str(&format!("{} {} HTTP/1.1\r\n", opts.method, uri.path));
    head.push_str(&format!("Host: {}\r\n", uri.host));
    let mut have_len = false;
    let mut have_conn = false;
    for (k, v) in &opts.headers {
        let kl = k.to_ascii_lowercase();
        if kl == "content-length" {
            have_len = true;
        }
        if kl == "connection" {
            have_conn = true;
        }
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    if !have_len && !opts.body.is_empty() {
        head.push_str(&format!("Content-Length: {}\r\n", opts.body.len()));
    }
    if !have_conn {
        head.push_str("Connection: close\r\n");
    }
    head.push_str("\r\n");

    let mut out = head.into_bytes();
    out.extend_from_slice(&opts.body);
    Ok(out)
}

// ---- response parsing ----

fn make_response(lua: &Lua, buf: &[u8]) -> mlua::Result<Table> {
    // Find the end of the header block (\r\n\r\n).
    let split = find_subslice(buf, b"\r\n\r\n")
        .ok_or_else(|| err("http: malformed response (no header terminator)".into()))?;
    let head = &buf[..split];
    let body = &buf[split + 4..];
    let head_str = std::str::from_utf8(head)
        .map_err(|e| err(format!("http: response headers not UTF-8: {e}")))?;

    let mut lines = head_str.split("\r\n");
    let status_line = lines
        .next()
        .ok_or_else(|| err("http: empty status line".into()))?;
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| err("http: malformed status line".into()))?
        .parse()
        .map_err(|_| err(format!("http: bad status code in {status_line:?}")))?;

    let headers = lua.create_table()?;
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.raw_set(k.trim().to_ascii_lowercase(), v.trim())?;
        }
    }

    let resp = lua.create_table()?;
    resp.raw_set("status", status)?;
    resp.raw_set("headers", headers)?;
    resp.raw_set("body", lua.create_string(body)?)?;
    Ok(resp)
}

fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len())
        .position(|w| w == needle)
}

// ---- option + error helpers ----

/// Parsed `request_uri` options (owned, so safe to hold across `.await`).
struct RequestOpts {
    method: String,
    body: Vec<u8>,
    headers: Vec<(String, String)>,
    verify: Option<bool>,
    timeout_ms: Option<u64>,
}

impl mlua::FromLua for RequestOpts {
    fn from_lua(v: Value, _lua: &Lua) -> mlua::Result<Self> {
        let Value::Table(t) = v else {
            return Ok(RequestOpts::default());
        };
        let method: String = t.get("method").unwrap_or_else(|_| "GET".into());
        let body = match t.get::<LuaString>("body") {
            Ok(s) => s.as_bytes().to_vec(),
            Err(_) => Vec::new(),
        };
        let mut headers = Vec::new();
        if let Ok(h) = t.get::<Table>("headers") {
            for pair in h.pairs::<String, LuaString>() {
                let (k, v) = pair?;
                headers.push((k, v.to_str()?.to_owned()));
            }
        }
        let verify = t.get("verify").ok();
        let timeout_ms = t.get("timeout").ok();
        Ok(RequestOpts { method, body, headers, verify, timeout_ms })
    }
}

impl Default for RequestOpts {
    fn default() -> Self {
        Self { method: "GET".into(), body: Vec::new(), headers: Vec::new(), verify: None, timeout_ms: None }
    }
}

/// Wrap a future in an optional timeout (ms). Returns `None` on timeout.
async fn connect_bounded<F>(timeout_ms: Option<u64>, fut: F) -> Option<F::Output>
where
    F: std::future::Future,
{
    match timeout_ms {
        Some(ms) => tokio::time::timeout(std::time::Duration::from_millis(ms), fut)
            .await
            .ok(),
        None => Some(fut.await),
    }
}

fn err(msg: String) -> mlua::Error {
    mlua::Error::RuntimeError(msg)
}
