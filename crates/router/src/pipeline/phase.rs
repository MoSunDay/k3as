//! The openresty phase model: an ordered set of named Lua phase functions.
//!
//! Order: per-request `rewrite` -> `access` -> `content` (the *generative*
//! phases, any of which may short-circuit via `ngx.exit`); then `header_filter`
//! -> `body_filter` (the *filter* phases, always run); then `log` (fire and
//! forget). `init_worker` runs once at boot, not per request. `balancer`
//! (T5.4) runs during upstream selection in the proxy data plane.

use mlua::Function;

/// A named phase hook. The canonical openresty subset; `balancer` runs during
/// upstream selection ([`crate::proxy`]) — it is *not* a generative phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Phase {
    /// Runs once at worker boot (shared-state setup). Not per-request.
    InitWorker,
    /// First per-request phase; may rewrite the URI / short-circuit.
    Rewrite,
    /// Access control; may deny via `ngx.exit(40x)`.
    Access,
    /// Produces the response body + status.
    Content,
    /// Mutates the assembled response headers.
    HeaderFilter,
    /// Transforms the response body chunk(s).
    BodyFilter,
    /// Post-response logging (fire and forget).
    Log,
    /// Upstream peer selection (`balancer_by_lua`, T5.4). Driven by the proxy
    /// data plane before forwarding; the default Rust round-robin balancer
    /// applies when this phase is absent.
    Balancer,
    /// TLS certificate selection (`ssl_certificate_by_lua`, T5.4 Scope
    /// B). Runs during the TLS handshake — the SNI hook. Driven by the
    /// listener-side rustls cert resolver; *not* a generative phase.
    SslCertificate,
}

impl Phase {
    /// The per-request generative phases in run order.
    pub(crate) const GENERATIVE: [Phase; 3] = [Phase::Rewrite, Phase::Access, Phase::Content];

    /// The openresty name (used to label tracing + error messages).
    pub fn label(self) -> &'static str {
        match self {
            Phase::InitWorker => "init_worker_by_lua",
            Phase::Rewrite => "rewrite_by_lua",
            Phase::Access => "access_by_lua",
            Phase::Content => "content_by_lua",
            Phase::HeaderFilter => "header_filter_by_lua",
            Phase::BodyFilter => "body_filter_by_lua",
            Phase::Log => "log_by_lua",
            Phase::Balancer => "balancer_by_lua",
            Phase::SslCertificate => "ssl_certificate_by_lua",
        }
    }
}

/// An optional Lua function registered for one [`Phase`]. Per-request phases are
/// optional; absent phases are skipped.
#[derive(Clone)]
pub(crate) struct PhaseSlot {
    pub phase: Phase,
    pub func: Function,
}

/// Ordered collection of registered per-request phase functions.
#[derive(Default)]
pub(crate) struct PhaseList {
    slots: Vec<PhaseSlot>,
}

impl PhaseList {
    pub(crate) fn set(&mut self, phase: Phase, func: Function) {
        if let Some(slot) = self.slots.iter_mut().find(|s| s.phase == phase) {
            slot.func = func;
        } else {
            self.slots.push(PhaseSlot { phase, func });
        }
    }

    pub(crate) fn get(&self, phase: Phase) -> Option<&Function> {
        self.slots
            .iter()
            .find(|s| s.phase == phase)
            .map(|s| &s.func)
    }
}

#[cfg(test)]
mod tests {
    use super::Phase;

    #[test]
    fn phase_ordering_is_stable() {
        assert_eq!(Phase::GENERATIVE[2], Phase::Content);
        assert_ne!(Phase::Rewrite, Phase::Access);
    }

    #[test]
    fn phase_enum_covers_openresty_subset() {
        let all = [
            Phase::InitWorker,
            Phase::Rewrite,
            Phase::Access,
            Phase::Content,
            Phase::HeaderFilter,
            Phase::BodyFilter,
            Phase::Log,
            Phase::Balancer,
            Phase::SslCertificate,
        ];
        assert_eq!(all.len(), 9);
        assert_eq!(Phase::SslCertificate.label(), "ssl_certificate_by_lua");
        // SslCertificate is intentionally NOT a generative phase.
        assert!(!Phase::GENERATIVE.contains(&Phase::SslCertificate));
        assert_eq!(Phase::Content.label(), "content_by_lua");
        assert_eq!(Phase::Balancer.label(), "balancer_by_lua");
        // Balancer is intentionally NOT a generative phase.
        assert!(!Phase::GENERATIVE.contains(&Phase::Balancer));
    }
}
