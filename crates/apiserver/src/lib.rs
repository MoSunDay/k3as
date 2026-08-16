//! Kubernetes API server HTTP layer (TODO **T1.2**).
//!
//! Serves byte-correct API discovery (T1.2a) plus REST CRUD + watch over a
//! [`storage::StorageBackend`] (T1.2b):
//!   - discovery: `GET /api`, `/apis`, `/api/v1`, `/apis/<g>/<v>`
//!   - CRUD: `POST`/`GET`/`PUT`/`DELETE`/`PATCH` on resource collections + items
//!   - watch: `GET /<collection>?watch=1` -> chunked `application/json` stream
//!
//! The discovery bodies are produced by [`api::discovery`]; the CRUD handlers
//! are a transport wrapper over [`storage`]. The wire format is JSON-only for
//! v1 (decision **Q10**). TLS (rustls) + real kubectl auth/interop are
//! deferred to **T1.3** (ADR **Q11**); acceptance for T1.2b is HTTP-level
//! round-trip against the embedded store (no real etcd needed — ADR **Q17**).
#![forbid(unsafe_code)]

mod app;
mod apply;
pub(crate) mod binding;
mod collection;
mod discovery_handlers;
mod error;
mod item;
mod serve;
mod state;

pub use app::api_app;
pub use serve::serve;

/// Discovery-only router kept for backward compatibility with the T1.2a tests:
/// builds the full app over a throwaway embedded store. Prefer [`api_app`].
pub fn discovery_app(
    registry: api::SchemaRegistry,
    server_address: impl Into<String>,
) -> axum::Router {
    use std::sync::Arc;
    let store: Arc<dyn storage::StorageBackend> = Arc::new(storage::EmbeddedStorage::new());
    api_app(Arc::new(registry), store, server_address.into())
}
