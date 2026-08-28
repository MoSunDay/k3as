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
//! [`Shutdown`] stops the accept loop immediately; in-flight connection
//! tasks are dropped when the plane runtime shuts down.

use std::net::SocketAddr;

use axum::extract::ConnectInfo;
use hyper_util::rt::{TokioExecutor, TokioIo};
use infra::Shutdown;
use tokio::net::TcpListener;
use tower::Service;

/// Bind `addr` for the gateway (loopback `:0` keeps integration tests
/// collision-free).
pub(crate) async fn bind(addr: SocketAddr) -> std::io::Result<TcpListener> {
    TcpListener::bind(addr).await
}

/// Accept loop: serves every connection through `svc` until `shutdown`
/// fires. `svc` is cloned per connection (tower convention); the
/// [`ConnectInfo`] extension openrusty's fallback handler requires is
/// inserted from the accepted remote address.
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
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => return,
            accepted = listener.accept() => match accepted {
                Ok((stream, remote)) => {
                    let svc = svc.clone();
                    tokio::spawn(async move {
                        let _ = stream.set_nodelay(true);
                        let builder =
                            hyper_util::server::conn::auto::Builder::new(TokioExecutor::new());
                        let service = hyper::service::service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
                            let svc = svc.clone();
                            async move {
                                let (mut parts, body) = req.into_parts();
                                parts.extensions.insert(ConnectInfo(remote));
                                let req =
                                    axum::extract::Request::from_parts(parts, axum::body::Body::new(body));
                                let mut svc = svc;
                                let resp = Service::call(&mut svc, req).await?;
                                Ok::<_, std::convert::Infallible>(resp)
                            }
                        });
                        let conn =
                            builder.serve_connection_with_upgrades(TokioIo::new(stream), service);
                        if let Err(e) = conn.await {
                            tracing::debug!(target: "init-pro", error = %e, "gateway connection ended");
                        }
                    });
                }
                Err(e) => {
                    tracing::debug!(target: "init-pro", error = %e, "gateway accept failed")
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use crate::endpoints::EndpointsResolver;
    use crate::gate::{GateRoute, GatedRouter};
    use crate::route::UpstreamRef;
    use std::collections::BTreeMap;

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
        let (itx, irx) =
            tokio::sync::watch::channel(Arc::new(BTreeMap::default()));
        drop((tx, itx));
        EndpointsResolver {
            rx,
            ingresses: irx,
        }
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
                let Ok((mut s, _)) = echo.accept().await else { break };
                let _ = s
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                    .await;
            }
        });

        let resolver = resolver_with_peer(peer);
        let listen: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let listener = bind(listen).await.unwrap();
        let bound = listener.local_addr().unwrap();
        let up = UpstreamRef::port("default/web", 80);
        let cfg = crate::gateway_config::nodeport_config(bound, &up, &[peer]);
        let app = openrusty_server::state::from_config(cfg, std::path::PathBuf::from("probe"))
            .expect("bootstrap");
        let gated = GatedRouter::new(
            openrusty_server::app::router(app),
            GateRoute::Fixed(up),
            resolver,
        );
        let shutdown = Shutdown::new();
        tokio::spawn(serve_until(listener, gated, shutdown));

        // Raw HTTP/1.1 client.
        let mut s = tokio::net::TcpStream::connect(bound).await.unwrap();
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        s.write_all(b"GET / HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut raw = String::new();
        s.read_to_string(&mut raw).await.unwrap();
        assert!(raw.contains("200"), "expected 200, got: {raw}");
        assert!(
            raw.split("\r\n\r\n").nth(1).unwrap_or_default().contains("ok"),
            "expected the echo backend's body, got: {raw}"
        );
    }
}
