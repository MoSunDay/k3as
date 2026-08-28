//! The gateway 503 gate (workstream A, decision **Q32**; behavior parity
//! with the Lua-era proxy under **Q28**).
//!
//! openrusty answers `502` when a route matches but its upstream has no
//! live peers; the Lua-era NodePort plane answered `503 "no upstream peers
//! available"` (text/plain) while Endpoints were empty, and
//! `service-traffic-e2e.sh` ST2/ST4 gate on that. This tower service wraps
//! the openrusty router and short-circuits to the same 503 whenever the
//! request's target upstream resolves to zero peers — before openrusty's
//! reverse proxy sees it. Requests with no matching route pass through and
//! keep openrusty's own 404.

use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use axum::extract::Request;
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::Response;
use tower::Service;

use crate::endpoints::{resolve_with_state, EndpointsResolver};
use crate::route::{RouteTable, UpstreamRef};

/// Parity with the Lua-era `conn::error_response(503, ...)` body (Q28).
const NO_PEERS_BODY: &str = "no upstream peers available";

/// How a request maps onto an upstream *before* proxying.
#[derive(Clone)]
pub(crate) enum GateRoute {
    /// NodePort listener: every request on the port targets one Service port.
    Fixed(UpstreamRef),
    /// Ingress gateway: consult the live compiled route table (host + path).
    Table(Arc<tokio::sync::watch::Receiver<Arc<RouteTable>>>),
}

/// Router + gate: an openrusty [`axum::Router`] behind the 503 short-circuit.
#[derive(Clone)]
pub(crate) struct GatedRouter {
    inner: axum::Router,
    route: GateRoute,
    resolver: EndpointsResolver,
}

impl GatedRouter {
    pub(crate) fn new(inner: axum::Router, route: GateRoute, resolver: EndpointsResolver) -> Self {
        Self {
            inner,
            route,
            resolver,
        }
    }

    /// The upstream this request would target, if any.
    fn target(&self, req: &Request) -> Option<UpstreamRef> {
        match &self.route {
            GateRoute::Fixed(up) => Some(up.clone()),
            GateRoute::Table(rx) => {
                let table = rx.borrow();
                let host = req
                    .headers()
                    .get(header::HOST)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("");
                let path = req.uri().path();
                table
                    .lookup(host, path)
                    .map(|r| r.upstream.clone())
                    .or_else(|| table.default_upstream().cloned())
            }
        }
    }

    /// 503 short-circuit when the target resolves to zero peers.
    fn short_circuit(&self, req: &Request) -> Option<Response> {
        let target = self.target(req)?;
        let state = self.resolver.snapshot();
        if resolve_with_state(&state, &target).is_empty() {
            Some(no_peers_response())
        } else {
            None
        }
    }
}

/// The Q28-parity 503: text/plain, no upstream peers available.
pub(crate) fn no_peers_response() -> Response {
    Response::builder()
        .status(StatusCode::SERVICE_UNAVAILABLE)
        .header(header::CONTENT_TYPE, HeaderValue::from_static("text/plain"))
        .body(axum::body::Body::from(NO_PEERS_BODY))
        .expect("static 503 response builds")
}

impl Service<Request> for GatedRouter {
    type Response = Response;
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<Response, Infallible>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
        <axum::Router as Service<Request>>::poll_ready(&mut self.inner, cx)
    }

    fn call(&mut self, req: Request) -> Self::Future {
        if let Some(resp) = self.short_circuit(&req) {
            return Box::pin(async move { Ok(resp) });
        }
        let mut inner = self.inner.clone();
        Box::pin(async move {
            let resp = Service::call(&mut inner, req).await?;
            Ok(resp)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::endpoints::ResolverState;
    use crate::route::{HostMatcher, PathMatcher, RouteRule};

    /// A resolver over an empty cluster (no Services, no Endpoints).
    fn empty_resolver() -> EndpointsResolver {
        let (state_tx, state_rx) =
            tokio::sync::watch::channel(Arc::new(ResolverState::default()));
        let (_, ingress_rx) =
            tokio::sync::watch::channel(Arc::new(crate::ingress_watch::IngressMap::default()));
        drop(state_tx);
        EndpointsResolver {
            rx: state_rx,
            ingresses: ingress_rx,
        }
    }

    fn fixed(up: UpstreamRef) -> GateRoute {
        GateRoute::Fixed(up)
    }

    fn req(host: &str, path: &str) -> Request {
        Request::builder()
            .uri(path)
            .header(header::HOST, host)
            .body(axum::body::Body::empty())
            .unwrap()
    }

    fn hostless_table(upstream: &str) -> RouteTable {
        let mut t = RouteTable::new();
        t.push(RouteRule {
            host: HostMatcher::Any,
            path: PathMatcher::new("Prefix", "/"),
            upstream: UpstreamRef::port(upstream, 80),
            origin: "test".into(),
        });
        t.finalise()
    }

    /// The gate must answer the Lua-era 503 (body + content type) while the
    /// NodePort Service has no Endpoints (ST2/ST4 parity, Q28).
    #[test]
    fn short_circuits_to_503_when_peers_are_empty() {
        let resolver = empty_resolver();
        let gated = GatedRouter::new(axum::Router::new(), fixed(UpstreamRef::port("default/web", 80)), resolver);
        let resp = gated.short_circuit(&req("anything.example", "/")).unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            resp.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/plain"
        );
    }

    /// No route match -> pass through (openrusty owns the 404).
    #[test]
    fn passes_through_when_no_route_matches() {
        let resolver = empty_resolver();
        let (table_tx, table_rx) = tokio::sync::watch::channel(Arc::new(RouteTable::new()));
        let gated = GatedRouter::new(
            axum::Router::new(),
            GateRoute::Table(Arc::new(table_rx)),
            resolver,
        );
        drop(table_tx);
        assert!(gated.short_circuit(&req("h.io", "/x")).is_none());
    }

    /// Table gate: host+path lookup hits the rule's upstream; the default
    /// backend catches host-less requests.
    #[test]
    fn table_gate_resolves_via_host_and_path() {
        let resolver = empty_resolver();
        let (table_tx, table_rx) =
            tokio::sync::watch::channel(Arc::new(hostless_table("default/web")));
        let gated = GatedRouter::new(
            axum::Router::new(),
            GateRoute::Table(Arc::new(table_rx)),
            resolver,
        );
        // Any host matches the host-less rule -> target has no peers -> 503.
        assert!(gated.short_circuit(&req("h.io", "/")).is_some());
        drop(table_tx);
    }

    /// Byte parity of the 503 body with the Lua-era proxy (Q28).
    #[tokio::test]
    async fn body_matches_lua_era_parity_text() {
        let resp = no_peers_response();
        let bytes = axum::body::to_bytes(resp.into_body(), 1024)
            .await
            .expect("read 503 body");
        assert_eq!(&bytes[..], NO_PEERS_BODY.as_bytes());
    }
}
