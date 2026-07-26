//! Cosocket: `ngx.socket.tcp` — async TCP from Lua (TODO **T5.2b**).
//!
//! A cosocket is a coroutine-friendly TCP socket: `connect`/`send`/`receive`
//! yield at Rust `await` points, so other requests' coroutines run while a
//! socket blocks — the openresty model. It is the foundation for
//! `resty.http` (T5.3) and upstream proxying (T5.4).
//!
//! # Binding
//! Unlike `ngx.req`/`ngx.header` (read via the global `ngx` table and so
//! needing the Q13 coroutine-local store), a cosocket is a Lua **value** held
//! in a phase-local — it is naturally per-request. No app-data wiring needed.
//!
//! # Minimal subset (openresty parity deferred to backlog)
//! `connect(host, port)`, `send(s)`, `receive(n)` (exact bytes) /
//! `receive("*l"|nil)` (one line), `settimeout(ms)`, `close()`. Errors raise a
//! Lua error rather than returning `nil, err` (openresty's multi-return); the
//! `setoption`/`sslhandshake`/`receiveuntil` surface is T5.2+ backlog.
//!
//! # Borrow discipline
//! Each I/O method **takes the stream out** of its `RefCell`, awaits, then
//! puts it back — so no `RefCell` guard is ever held across an `await` (which
//! would either deadlock or violate the `'static` future bound).

use std::cell::RefCell;
use std::time::Duration;

use mlua::{Lua, UserData, UserDataMethods, UserDataRef};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// The Lua-visible cosocket userdata.
pub struct Cosocket {
    inner: RefCell<Inner>,
}

struct Inner {
    stream: Option<TcpStream>,
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
        // Minimal no-op stubs: keepalive pooling is T5.4 backlog.
        methods.add_async_method("setkeepalive", |_, _, _: Option<u64>| async { Ok(1) });
        methods.add_async_method("close", close);
    }
}

/// `sock:connect(host, port)` -> `1`.
async fn connect(_lua: Lua, this: UserDataRef<Cosocket>, (host, port): (String, u16)) -> mlua::Result<i64> {
    let stream = TcpStream::connect((host.as_str(), port))
        .await
        .map_err(|e| runtime(format!("cosocket connect: {e}")))?;
    {
        let mut g = this.inner.borrow_mut();
        if g.closed {
            return Err(runtime("cosocket closed"));
        }
        g.stream = Some(stream);
    }
    Ok(1)
}

/// `sock:send(s [, ...])` -> total bytes sent.
async fn send(_lua: Lua, this: UserDataRef<Cosocket>, chunks: mlua::Variadic<mlua::LuaString>) -> mlua::Result<i64> {
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
async fn receive(lua: Lua, this: UserDataRef<Cosocket>, spec: ReceiveSpec) -> mlua::Result<mlua::LuaString> {
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

/// `sock:close()` -> `1`.
async fn close(_lua: Lua, this: UserDataRef<Cosocket>, (): ()) -> mlua::Result<i64> {
    let mut g = this.inner.borrow_mut();
    g.closed = true;
    g.stream = None; // dropping the TcpStream closes it
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

fn take_stream(this: &UserDataRef<Cosocket>) -> mlua::Result<TcpStream> {
    let mut g = this.inner.borrow_mut();
    if g.closed {
        return Err(runtime("cosocket closed"));
    }
    g.stream
        .take()
        .ok_or_else(|| runtime("cosocket not connected"))
}

fn put_back(this: &UserDataRef<Cosocket>, stream: TcpStream) {
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
