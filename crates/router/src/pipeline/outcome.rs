//! The [`PhaseOutcome`] value type and the [`build_response`] serializer.

use bytes::Bytes;
use http::{HeaderValue, Response, StatusCode};

/// Outcome of driving one request through the pipeline (after every phase).
pub struct PhaseOutcome {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub said: bool,
}

impl PhaseOutcome {
    /// Snapshot the final per-request state into an outcome.
    pub(crate) fn from_ctx(ctx: &std::cell::RefCell<crate::context::RequestContext>) -> Self {
        let g = ctx.borrow();
        PhaseOutcome {
            status: g.status,
            headers: g.resp_headers.clone(),
            body: g.body.clone(),
            said: g.said,
        }
    }
}

/// Turn a [`PhaseOutcome`] into an [`http::Response`], applying the openresty
/// default `Content-Type: text/plain` when only `ngx.say`/`ngx.print` were used
/// and no content-type was set, plus `Content-Length`.
pub fn build_response(out: PhaseOutcome) -> Response<Bytes> {
    let mut builder = Response::builder().status(
        StatusCode::from_u16(out.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
    );
    let mut has_content_type = false;
    for (name, value) in &out.headers {
        if name == "content-type" {
            has_content_type = true;
        }
        if let (Ok(n), Ok(v)) = (
            http::HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            builder = builder.header(n, v);
        }
    }
    if out.said && !has_content_type && !out.body.is_empty() {
        builder = builder.header(http::header::CONTENT_TYPE, "text/plain");
    }
    let body = Bytes::from(out.body);
    builder = builder.header(http::header::CONTENT_LENGTH, body.len());
    builder.body(body).unwrap_or_else(|_| Response::new(Bytes::new()))
}
