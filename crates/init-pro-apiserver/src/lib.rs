//! Kubernetes API server HTTP layer (TODO **T1.2a**).
//!
//! Serves byte-correct API discovery over HTTP from a [`SchemaRegistry`]:
//!   - `GET /api`                  -> `APIVersions`     (core group)
//!   - `GET /apis`                 -> `APIGroupList`    (non-core groups)
//!   - `GET /api/v1`               -> `APIResourceList` (core/v1 index)
//!   - `GET /apis/:group/:version` -> `APIResourceList` (per group/version)
//!
//! The discovery bodies are produced by [`init_pro_api::discovery`], which is
//! already unit-tested for byte fidelity against upstream `meta/v1`. These
//! handlers are a thin transport wrapper: build the document, hand it to
//! `axum::Json` (which sets `Content-Type: application/json`, the sole wire
//! codec per decision **Q10**).
//!
//! # Scope
//!
//! Discovery-only. No CRUD, watch, or persistence: the store trait +
//! etcd-backed CRUD lands in **T1.2b** (needs T2.2). TLS (rustls) and real
//! kubectl interop are deferred — acceptance for T1.2a is `curl`
//! byte-equivalence over plain HTTP on the loopback (see ADR **Q11**). The HTTP
//! framework choice (axum) is shared with the Router data plane (T5.2 "hyper
//! body streaming"), so it de-risks both the critical and the de-risk paths.
#![forbid(unsafe_code)]

mod discovery_handlers;
mod serve;

pub use discovery_handlers::discovery_app;
pub use serve::serve;
