//! Cosocket: `ngx.socket.tcp` — async TCP from Lua (T5.2b), with TLS upgrade
//! (T5.3 Scope B `sslhandshake`).
//!
//! A cosocket is a coroutine-friendly TCP socket: `connect`/`send`/`receive`
//! yield at Rust `await` points, so other requests' coroutines run while a
//! socket blocks — the openresty model. `sslhandshake` upgrades a plaintext
//! connection to TLS (client side); it is the foundation for `resty.http` over
//! HTTPS.
//!
//! # Binding
//! Unlike `ngx.req`/`ngx.header` (read via the global `ngx` table and so
//! needing the Q13 coroutine-local store), a cosocket is a Lua **value** held
//! in a phase-local — it is naturally per-request. No app-data wiring needed.
//!
//! # Borrow discipline
//! Each I/O method **takes the stream out** of its `RefCell`, awaits, then
//! puts it back — so no `RefCell` guard is ever held across an `await` (which
//! would either deadlock or violate the `'static` future bound).

use std::cell::RefCell;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use mlua::{Lua, UserData, UserDataMethods, UserDataRef};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::TcpStream;

/// The underlying byte stream of a cosocket: plaintext TCP or a TLS session
/// layered over TCP. Both implement [`AsyncRead`] + [`AsyncWrite`]; the enum
/// delegates, so every cosocket method works unchanged after `sslhandshake`.
enum ConnStream {
    Plain(TcpStream),
    Tls(Box<tokio_rustls::client::TlsStream<TcpStream>>),
}

impl AsyncRead for ConnStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            ConnStream::Plain(s) => Pin::new(s).poll_read(cx, buf),
            ConnStream::Tls(s) => Pin::new(&mut **s).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for ConnStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            ConnStream::Plain(s) => Pin::new(s).poll_write(cx, buf),
            ConnStream::Tls(s) => Pin::new(&mut **s).poll_write(cx, buf),
        }
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            ConnStream::Plain(s) => Pin::new(s).poll_flush(cx),
            ConnStream::Tls(s) => Pin::new(&mut **s).poll_flush(cx),
        }
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            ConnStream::Plain(s) => Pin::new(s).poll_shutdown(cx),
            ConnStream::Tls(s) => Pin::new(&mut **s).poll_shutdown(cx),
        }
    }
}

/// The Lua-visible cosocket userdata.
pub struct Cosocket {
    inner: RefCell<Inner>,
}

struct Inner {
    stream: Option<ConnStream>,
    timeout_ms: Option<u64>,
    closed: bool,
}

impl Cosocket {
    pub fn new() -> Self {
        Self {
            inner: RefCell::new(Inner {
                stream: None,
                timeout_ms: None,
                closed: false,
            }),
        }
    }
}

impl UserData for Cosocket {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_async_method("connect", connect);
        methods.add_async_method("send", send);
        methods.add_async_method("receive", receive);
        methods.add_async_method("settimeout", settimeout);
        methods.add_async_method("sslhandshake", sslhandshake);
        // Minimal no-op stubs: keepalive pooling is T5.4 backlog.
        methods.add_async_method("setkeepalive", |_, _, _: Option<u64>| async { Ok(1) });
        methods.add_async_method("close", close);
    }
}

/// `sock:connect(host, port)` -> `1`.
async fn connect(
    _lua: Lua,
    this: UserDataRef<Cosocket>,
    (host, port): (String, u16),
) -> mlua::Result<i64> {
    let stream = TcpStream::connect((host.as_str(), port))
        .await
        .map_err(|e| runtime(format!("cosocket connect: {e}")))?;
    {
        let mut g = this.inner.borrow_mut();
        if g.closed {
            return Err(runtime("cosocket closed"));
        }
        g.stream = Some(ConnStream::Plain(stream));
    }
    Ok(1)
}

/// `sock:send(s [, ...])` -> total bytes sent.
async fn send(
    _lua: Lua,
    this: UserDataRef<Cosocket>,
    chunks: mlua::Variadic<mlua::LuaString>,
) -> mlua::Result<i64> {
    let mut payload = Vec::new();
    for c in chunks {
        payload.extend_from_slice(&c.as_bytes());
    }
    let n = payload.len();
    let timeout_ms = this.inner.borrow().timeout_ms;

    let mut stream = take_stream(&this)?;
    let r = time_bounded(timeout_ms, stream.write_all(&payload)).await;
    match r {
        Ok(Ok(_)) => {
            put_back(&this, stream);
            Ok(n as i64)
        }
        Ok(Err(e)) => fail(&this, format!("cosocket send: {e}")),
        Err(()) => fail(&this, "cosocket send: timeout".into()),
    }
}

/// `sock:receive(n)` -> exactly `n` bytes; `sock:receive("*l"|nil)` -> one line.
async fn receive(
    lua: Lua,
    this: UserDataRef<Cosocket>,
    spec: ReceiveSpec,
) -> mlua::Result<mlua::LuaString> {
    let bytes = match spec {
        ReceiveSpec::Bytes(n) => recv_exact(&this, n).await?,
        ReceiveSpec::Line => recv_line(&this).await?,
    };
    lua.create_string(&bytes)
}

/// `sock:settimeout(ms)` -> `1`; `nil`/0 clears the timeout.
async fn settimeout(_lua: Lua, this: UserDataRef<Cosocket>, ms: Option<u64>) -> mlua::Result<i64> {
    this.inner.borrow_mut().timeout_ms = ms.filter(|&m| m > 0);
    Ok(1)
}

/// Parsed `sslhandshake` options table.
struct SslOpts {
    server_name: String,
    verify: bool,
}

impl mlua::FromLua for SslOpts {
    fn from_lua(value: mlua::Value, _lua: &Lua) -> mlua::Result<Self> {
        match value {
            mlua::Value::Table(t) => {
                let server_name: String = t.get("server_name")?;
                let verify: bool = t.get("verify").unwrap_or(true);
                Ok(SslOpts {
                    server_name,
                    verify,
                })
            }
            mlua::Value::String(s) => Ok(SslOpts {
                server_name: s.to_str()?.to_owned(),
                verify: true,
            }),
            mlua::Value::Nil => Err(runtime("sslhandshake: server_name required")),
            other => Err(runtime(format!(
                "sslhandshake: expected table/string, got {other:?}"
            ))),
        }
    }
}

/// `sock:sslhandshake(opts)` -> `true`. `opts` is a table with `server_name`
/// (string, required) and `verify` (bool, default true). Upgrades the existing
/// plaintext connection to TLS (client side). openresty's `reuse_session` arg
/// is accepted-but-ignored (session resumption is backlog).
async fn sslhandshake(_lua: Lua, this: UserDataRef<Cosocket>, opts: SslOpts) -> mlua::Result<bool> {
    let stream = take_stream(&this)?;
    let plain = match stream {
        ConnStream::Plain(t) => t,
        ConnStream::Tls(_) => {
            put_back(&this, stream);
            return Err(runtime("cosocket already in TLS"));
        }
    };
    let cfg = crate::tls::build_client_config(opts.verify)
        .map_err(|e| runtime(format!("sslhandshake: client config: {e}")))?;
    let connector = tokio_rustls::TlsConnector::from(Arc::new(cfg));
    let name = rustls::pki_types::ServerName::try_from(opts.server_name.clone()).map_err(|e| {
        runtime(format!(
            "sslhandshake: bad server name {0:?}: {e}",
            opts.server_name
        ))
    })?;
    match connector.connect(name, plain).await {
        Ok(tls) => {
            put_back(&this, ConnStream::Tls(Box::new(tls)));
            Ok(true)
        }
        Err(e) => Err(runtime(format!("sslhandshake: {e}"))),
    }
}

/// `sock:close()` -> `1`.
async fn close(_lua: Lua, this: UserDataRef<Cosocket>, (): ()) -> mlua::Result<i64> {
    let mut g = this.inner.borrow_mut();
    g.closed = true;
    g.stream = None; // dropping the stream closes it
    Ok(1)
}

// ---- receive modes ----

async fn recv_exact(this: &UserDataRef<Cosocket>, n: usize) -> mlua::Result<Vec<u8>> {
    if n == 0 {
        return Ok(Vec::new());
    }
    let timeout_ms = this.inner.borrow().timeout_ms;
    let mut stream = take_stream(this)?;
    let mut buf = vec![0u8; n];
    match time_bounded(timeout_ms, stream.read_exact(&mut buf)).await {
        Ok(Ok(_)) => {
            put_back(this, stream);
            Ok(buf)
        }
        Ok(Err(e)) => fail(this, format!("cosocket receive: {e}")),
        Err(()) => fail(this, "cosocket receive: timeout".into()),
    }
}

async fn recv_line(this: &UserDataRef<Cosocket>) -> mlua::Result<Vec<u8>> {
    let timeout_ms = this.inner.borrow().timeout_ms;
    let mut stream = take_stream(this)?;
    let mut out = Vec::new();
    let mut one = [0u8; 1];
    loop {
        match time_bounded(timeout_ms, stream.read(&mut one)).await {
            Ok(Ok(0)) => break, // EOF terminates the line
            Ok(Ok(_)) => {
                out.push(one[0]);
                if one[0] == b'\n' {
                    break;
                }
            }
            Ok(Err(e)) => return fail(this, format!("cosocket receive: {e}")),
            Err(()) => return fail(this, "cosocket receive: timeout".into()),
        }
    }
    put_back(this, stream);
    Ok(out)
}

// ---- internals ----

enum ReceiveSpec {
    Bytes(usize),
    Line,
}

impl mlua::FromLua for ReceiveSpec {
    fn from_lua(value: mlua::Value, _lua: &Lua) -> mlua::Result<Self> {
        match value {
            mlua::Value::Integer(i) if i >= 0 => Ok(ReceiveSpec::Bytes(i as usize)),
            mlua::Value::Number(x) if x >= 0.0 => Ok(ReceiveSpec::Bytes(x as usize)),
            mlua::Value::String(s) if s.to_str()? == "*l" => Ok(ReceiveSpec::Line),
            mlua::Value::Nil => Ok(ReceiveSpec::Line),
            _ => Err(runtime("receive: bad argument")),
        }
    }
}

fn take_stream(this: &UserDataRef<Cosocket>) -> mlua::Result<ConnStream> {
    let mut g = this.inner.borrow_mut();
    if g.closed {
        return Err(runtime("cosocket closed"));
    }
    g.stream
        .take()
        .ok_or_else(|| runtime("cosocket not connected"))
}

fn put_back(this: &UserDataRef<Cosocket>, stream: ConnStream) {
    this.inner.borrow_mut().stream = Some(stream);
}

/// Drop the stream and return a RuntimeError (the connection is now dead).
fn fail<T>(this: &UserDataRef<Cosocket>, msg: String) -> mlua::Result<T> {
    this.inner.borrow_mut().stream = None;
    Err(runtime(msg))
}

fn runtime(msg: impl Into<String>) -> mlua::Error {
    mlua::Error::RuntimeError(msg.into())
}

/// Wrap a future in an optional timeout; `Err(())` means it elapsed.
async fn time_bounded<F, T>(timeout_ms: Option<u64>, fut: F) -> Result<T, ()>
where
    F: std::future::Future<Output = T>,
{
    match timeout_ms {
        Some(ms) => tokio::time::timeout(Duration::from_millis(ms), fut)
            .await
            .map_err(|_| ()),
        None => Ok(fut.await),
    }
}
