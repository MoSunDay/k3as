//! Listener + accept-loop seam for the openrusty gateway (workstream A /
//! decision **Q32**).
//!
//! `openrusty_server::h2c::serve` binds a concrete address internally and
//! its `serve_listener` helper is `pub(crate)`, so the router owns the
//! accept loop: bind (possibly `:0` for tests), then drive each accepted
//! TCP stream through a hyper-util auto connection (HTTP/1.1 + h2c
//! auto-detect) whose service is the gated openrusty router
//! ([`crate::gate::GatedRouter`]).
//!
//! Shutdown parity with the Lua-era proxy (Sprint 18, Q28): a closed
//! [`Shutdown`] stops the accept loop immediately. T5.4 hardening: the
//! connection tasks are TRACKED (a `JoinSet`, no detached spawns), aborted
//! on shutdown and drained within a bounded [`DRAIN_GRACE`], so
//! [`serve_until`] returns once its connections are torn down instead of
//! leaving them to the plane runtime's teardown.
//!
//! Connection hygiene (T5.4 hardening): in-flight connections are capped
//! at [`MAX_CONNS`] per listener (at the cap the stream is dropped with a
//! rate-limited warn) and every HTTP/1 connection gets an explicit
//! [`HEADER_READ_TIMEOUT`], so idle or slow-header clients cannot pin tasks
//! forever.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::ConnectInfo;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use infra::Shutdown;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinSet;
use tower::Service;

/// In-flight connection cap per listener: beyond it the accept loop drops
/// the stream (the client sees a reset) instead of piling up tasks.
pub(crate) const MAX_CONNS: usize = 4096;

/// Explicit header read timeout for HTTP/1 connections (slowloris bound);
/// HTTP/2 keeps hyper's own default.
pub(crate) const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(30);

/// Drain grace after `shutdown` fires (matches the apiserver's): the
/// connection tasks are aborted first, so this only bounds the join.
pub(crate) const DRAIN_GRACE: Duration = Duration::from_secs(2);

/// Minimum spacing between two at-cap warns (the cap is a sustained
/// condition; one log line per dropped connection would be the DoS).
pub(crate) const CAP_WARN_INTERVAL: Duration = Duration::from_secs(1);

/// Bind `addr` for the gateway (loopback `:0` keeps integration tests
/// collision-free).
pub(crate) async fn bind(addr: SocketAddr) -> std::io::Result<TcpListener> {
    TcpListener::bind(addr).await
}

/// Accept loop: serves every connection through `svc` until `shutdown`
/// fires. `svc` is cloned per connection (tower convention); the
/// [`ConnectInfo`] extension openrusty's fallback handler requires is
/// inserted from the accepted remote address.
///
/// Every connection task is tracked in a [`JoinSet`] (finished ones are
/// reaped as they end); on shutdown they are aborted and drained within
/// [`DRAIN_GRACE`], then the loop returns.
pub(crate) async fn serve_until<S>(listener: TcpListener, svc: S, shutdown: Shutdown)
where
    S: Service<
            axum::extract::Request,
            Response = axum::response::Response,
            Error = std::convert::Infallible,
        > + Clone
        + Send
        + 'static,
    S::Future: Send + 'static,
{
    serve_bounded(listener, svc, shutdown, MAX_CONNS).await
}

/// [`serve_until`] with an explicit connection cap (tests exercise the cap
/// with a tiny limit instead of [`MAX_CONNS`]).
async fn serve_bounded<S>(listener: TcpListener, svc: S, shutdown: Shutdown, max_conns: usize)
where
    S: Service<
            axum::extract::Request,
            Response = axum::response::Response,
            Error = std::convert::Infallible,
        > + Clone
        + Send
        + 'static,
    S::Future: Send + 'static,
{
    // One permit per in-flight connection; the permit rides inside the
    // connection task, so a finished (or aborted) task frees it.
    let conns = Arc::new(Semaphore::new(max_conns));
    let mut tasks: JoinSet<()> = JoinSet::new();
    let mut cap_warn: Option<Instant> = None;
    loop {
        // Computed before the select: the reap branch's future borrows the
        // set for the whole iteration.
        let reaping = !tasks.is_empty();
        tokio::select! {
            // Reap a finished connection (keeps the set from growing with
            // already-ended tasks).
            Some(res) = tasks.join_next(), if reaping => {
                if let Err(e) = res {
                    tracing::debug!(target: "init-pro", error = %e, "gateway connection task ended abruptly");
                }
            }
            _ = shutdown.cancelled() => break,
            accepted = listener.accept() => match accepted {
                Ok((stream, remote)) => match Arc::clone(&conns).try_acquire_owned() {
                    Ok(permit) => {
                        let svc = svc.clone();
                        tasks.spawn(serve_connection(stream, remote, svc, permit));
                    }
                    Err(_) => {
                        // At the cap: drop the stream (the client sees a
                        // reset) and warn, rate-limited.
                        if warn_due(cap_warn, Instant::now()) {
                            cap_warn = Some(Instant::now());
                            tracing::warn!(
                                target: "init-pro",
                                max = max_conns,
                                "gateway connection cap reached; dropping connections"
                            );
                        }
                    }
                },
                Err(e) => {
                    tracing::debug!(target: "init-pro", error = %e, "gateway accept failed")
                }
            },
        }
    }
    drain(&mut tasks).await;
}

/// One accepted connection: the hyper-util auto connection over `stream`
/// (explicit [`HEADER_READ_TIMEOUT`]). The permit is held until the
/// connection ends, which is what bounds [`serve_bounded`]'s in-flight set.
async fn serve_connection<S>(
    stream: TcpStream,
    remote: SocketAddr,
    svc: S,
    _permit: OwnedSemaphorePermit,
) where
    S: Service<
            axum::extract::Request,
            Response = axum::response::Response,
            Error = std::convert::Infallible,
        > + Clone
        + Send
        + 'static,
    S::Future: Send + 'static,
{
    let _ = stream.set_nodelay(true);
    let mut builder = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new());
    // The timer is what arms the header timeout (hyper panics on an
    // unarmed one); the auto builder installs none by default.
    builder
        .http1()
        .timer(TokioTimer::new())
        .header_read_timeout(HEADER_READ_TIMEOUT);
    let service = hyper::service::service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
        let svc = svc.clone();
        async move {
            let (mut parts, body) = req.into_parts();
            parts.extensions.insert(ConnectInfo(remote));
            let req = axum::extract::Request::from_parts(parts, axum::body::Body::new(body));
            let mut svc = svc;
            let resp = Service::call(&mut svc, req).await?;
            Ok::<_, std::convert::Infallible>(resp)
        }
    });
    let conn = builder.serve_connection_with_upgrades(TokioIo::new(stream), service);
    if let Err(e) = conn.await {
        tracing::debug!(target: "init-pro", error = %e, "gateway connection ended");
    }
}

/// Shutdown teardown: abort every tracked connection task, then join them
/// with a bounded grace so [`serve_until`] always returns -- even if a task
/// is parked on a blocking-pool call.
async fn drain(tasks: &mut JoinSet<()>) {
    tasks.abort_all();
    let deadline = tokio::time::Instant::now() + DRAIN_GRACE;
    while !tasks.is_empty() {
        match tokio::time::timeout_at(deadline, tasks.join_next()).await {
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(_) => {
                tracing::debug!(
                    target: "init-pro",
                    remaining = tasks.len(),
                    "gateway drain grace exhausted; abandoning connection tasks"
                );
                break;
            }
        }
    }
}

/// At-cap warn spacing (pure; unit-tested): warn when none was emitted yet
/// or the last one is older than [`CAP_WARN_INTERVAL`].
fn warn_due(last_warn: Option<Instant>, now: Instant) -> bool {
    last_warn.is_none_or(|t| now.duration_since(t) >= CAP_WARN_INTERVAL)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::endpoints::EndpointsResolver;
    use crate::gate::{GateRoute, GatedRouter};
    use crate::route::UpstreamRef;
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// A resolver snapshot with one live peer for `default/web:80`.
    fn resolver_with_peer(peer: SocketAddr) -> EndpointsResolver {
        use crate::endpoints::{parse_endpoints, parse_service, ResolverState};
        let mut st = ResolverState::default();
        let svc = parse_service(&serde_json::json!({
            "metadata": {"name": "web", "namespace": "default"},
            "spec": {"type": "NodePort", "ports": [
                {"name": "http", "port": 80, "targetPort": 4242, "nodePort": 30080}]}
        }))
        .unwrap();
        let ep = parse_endpoints(&serde_json::json!({
            "metadata": {"name": "web", "namespace": "default"},
            "subsets": [{"addresses": [{"ip": peer.ip().to_string()}],
                         "ports": [{"name": "http", "port": 4242}]}]
        }))
        .unwrap();
        st.services.insert("default/web".into(), svc);
        st.endpoints.insert("default/web".into(), ep);
        let (tx, rx) = tokio::sync::watch::channel(Arc::new(st));
        let (itx, irx) = tokio::sync::watch::channel(Arc::new(BTreeMap::default()));
        drop((tx, itx));
        EndpointsResolver { rx, ingresses: irx }
    }

    /// Full serve-loop regression (Q32): gated openrusty listener + a real
    /// TCP client. Guards the `ConnectInfo<SocketAddr>` insertion and the
    /// gate's peer resolution end to end (a 503 here means the gate's
    /// UpstreamRef port no longer matches the Service port).
    #[tokio::test]
    async fn gated_listener_proxies_http() {
        // Live echo backend.
        let echo = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let peer = echo.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut s, _)) = echo.accept().await else {
                    break;
                };
                let _ = s
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                    )
                    .await;
            }
        });

        let resolver = resolver_with_peer(peer);
        let listen: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let listener = bind(listen).await.unwrap();
        let bound = listener.local_addr().unwrap();
        let up = UpstreamRef::port("default/web", 80);
        let cfg =
            crate::gateway_config::nodeport_config(bound, &up, &crate::split::uniform(vec![peer]));
        let app = openrusty_server::state::from_config(cfg, std::path::PathBuf::from("probe"))
            .expect("bootstrap");
        let gated = GatedRouter::new(
            openrusty_server::app::router(app),
            GateRoute::fixed(up),
            resolver,
        );
        let shutdown = Shutdown::new();
        tokio::spawn(serve_until(listener, gated, shutdown));

        // Raw HTTP/1.1 client.
        let mut s = tokio::net::TcpStream::connect(bound).await.unwrap();
        s.write_all(b"GET / HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut raw = String::new();
        s.read_to_string(&mut raw).await.unwrap();
        assert!(raw.contains("200"), "expected 200, got: {raw}");
        assert!(
            raw.split("\r\n\r\n")
                .nth(1)
                .unwrap_or_default()
                .contains("ok"),
            "expected the echo backend's body, got: {raw}"
        );
    }
    type OkFuture = std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<axum::response::Response, std::convert::Infallible>,
                > + Send,
        >,
    >;

    /// Always-200 handler for the lifecycle tests (no upstream).
    fn ok(_req: axum::extract::Request) -> OkFuture {
        Box::pin(async { Ok(axum::response::Response::new(axum::body::Body::empty())) })
    }

    /// Minimal always-200 service for the lifecycle tests (a named fn, so
    /// the `ServiceFn` type -- and its `Send` future -- is nameable).
    fn ok_service() -> tower::util::ServiceFn<fn(axum::extract::Request) -> OkFuture> {
        tower::service_fn(ok)
    }

    /// Read one HTTP/1 response head (up to the blank line).
    async fn read_head(s: &mut TcpStream) -> String {
        let mut raw = Vec::new();
        let mut buf = [0u8; 256];
        loop {
            let n = s.read(&mut buf).await.unwrap();
            assert!(n > 0, "eof before the response head");
            raw.extend_from_slice(&buf[..n]);
            if raw.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        String::from_utf8_lossy(&raw).into_owned()
    }

    /// T5.4 hardening: the at-cap warn fires on the first drop, then at
    /// most once per [`CAP_WARN_INTERVAL`].
    #[test]
    fn at_cap_warn_is_rate_limited() {
        let t0 = Instant::now();
        assert!(warn_due(None, t0), "the first drop always warns");
        assert!(!warn_due(Some(t0), t0 + Duration::from_millis(500)));
        assert!(warn_due(Some(t0), t0 + CAP_WARN_INTERVAL));
    }

    /// T5.4 hardening: an idle connection (connected, never sends a byte)
    /// must not hold `serve_until` open past the drain grace once shutdown
    /// fires, and must be torn down with the listener.
    #[tokio::test]
    async fn serve_until_returns_within_grace_with_an_idle_connection() {
        let listener = bind("127.0.0.1:0".parse().unwrap()).await.unwrap();
        let bound = listener.local_addr().unwrap();
        let shutdown = Shutdown::new();
        let serve = tokio::spawn(serve_until(listener, ok_service(), shutdown.clone()));

        // Let the accept loop admit the idle connection before firing.
        let mut idle = TcpStream::connect(bound).await.unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;

        let started = Instant::now();
        shutdown.trigger();
        tokio::time::timeout(DRAIN_GRACE + Duration::from_secs(1), serve)
            .await
            .expect("serve_until must return within the drain grace")
            .unwrap();
        assert!(
            started.elapsed() <= DRAIN_GRACE + Duration::from_secs(1),
            "drain took {:?}",
            started.elapsed()
        );
        let mut buf = [0u8; 8];
        let read = tokio::time::timeout(Duration::from_secs(1), idle.read(&mut buf)).await;
        assert!(
            !matches!(read, Ok(Ok(n)) if n > 0),
            "idle connection still serving after drain: {read:?}"
        );
    }

    /// T5.4 hardening: beyond the cap the stream is dropped (EOF/reset,
    /// never a parked task) while the holder's connection keeps serving.
    #[tokio::test]
    async fn connection_cap_drops_the_stream_beyond_the_limit() {
        let listener = bind("127.0.0.1:0".parse().unwrap()).await.unwrap();
        let bound = listener.local_addr().unwrap();
        let shutdown = Shutdown::new();
        let serve = tokio::spawn(serve_bounded(listener, ok_service(), shutdown.clone(), 1));

        // Holder: one keep-alive request -> response; the connection stays
        // open, so the single permit stays taken.
        let mut holder = TcpStream::connect(bound).await.unwrap();
        holder
            .write_all(b"GET / HTTP/1.1\r\nHost: t\r\n\r\n")
            .await
            .unwrap();
        let head = read_head(&mut holder).await;
        assert!(head.starts_with("HTTP/1.1 200"), "holder response: {head}");

        // Beyond the cap: accepted, then dropped -> EOF or reset, not a hang.
        let mut extra = TcpStream::connect(bound).await.unwrap();
        let mut buf = [0u8; 16];
        let read = tokio::time::timeout(Duration::from_secs(3), extra.read(&mut buf))
            .await
            .expect("a capped connection must not hang");
        assert!(
            matches!(read, Ok(0) | Err(_)),
            "no bytes may be served on a capped connection: {read:?}"
        );

        shutdown.trigger();
        tokio::time::timeout(DRAIN_GRACE + Duration::from_secs(1), serve)
            .await
            .expect("serve task returns within the drain grace")
            .unwrap();
    }
}
