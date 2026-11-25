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
#[cfg(test)]
mod auth_tests;
mod authn;
mod authz;
pub mod authz_bootstrap;
// S51 hygiene split (D0): the authz unit tests moved out of `authz.rs`
// to keep that file under the 400-line cap (the `pod_io_tests` precedent).
#[cfg(test)]
mod authz_tests;
pub(crate) mod binding;
mod collection;
mod discovery_handlers;
mod error;
mod item;
// S52-D0 hygiene split: the axum wrapper handlers moved out of `item.rs`
// (file-size cap; the shared do_* logic stays there).
mod item_http;
// T4.5 (Q65): `kubelet_hop` is PUBLIC so the node-side tunnel dialer
// (crates/cli/src/tunnel_dialer.rs) reuses the exact frozen wire contract
// (build_hop_request / parse_hop_response / call_kubelet) instead of
// re-implementing it.
pub mod kubelet_hop;
pub(crate) mod node_status;
pub(crate) mod pod_io;
#[cfg(test)]
mod pod_io_tests;
#[cfg(test)]
mod pod_io_wire_common;
#[cfg(test)]
mod pod_io_wire_resolution_tests;
#[cfg(test)]
mod pod_io_wire_tests;
pub(crate) mod pod_status;
mod pod_workspace;
mod serve;
mod service;
mod service_ip;
// T4.5 (Q65): the reverse relay tunnel — registry state machine + the
// agent-dialed HTTP surface (Q31 single-request relay over JSON).
// T4.5 (Q65): the wire types are re-exported PUBLIC because the
// node-side dialer (crates/cli/src/tunnel_dialer.rs) must parse
// `RelayRequest` and build `RelayResponse` with the EXACT same serde
// shape the apiserver emits (Q10 JSON-only wire contract).
mod tunnel_http;
mod tunnel_registry;
mod tunnel_wire;
pub use tunnel_registry::LONG_POLL_TIMEOUT;
// S53 / Q65 (review P1-③): `RegisterAck` joins the re-exports because the
// node-side dialer parses the register ack (park credential) with the
// EXACT same serde shape the apiserver emits (Q10 JSON-only wire contract).
pub use tunnel_wire::{RegisterAck, RelayRequest, RelayResponse};
// S51: the ClusterIP allocation unit tests live beside `service_ip.rs`
// (file-size cap; the `authz_tests` precedent).
#[cfg(test)]
mod service_ip_tests;
mod state;
mod tls;

pub use app::{api_app, api_app_with_workspace_nfs};
pub use authn::{Identity, TokenAuth};
pub use authz_bootstrap::bootstrap_rbac;
pub use serve::{serve, ServeOptions};
pub use service_ip::{ServiceCidr, DEFAULT_SERVICE_CIDR};
pub use tls::{acceptor, generate_self_signed, TlsListener, TlsMaterial};

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
