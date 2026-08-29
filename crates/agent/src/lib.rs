//! Agent seam crate (TODO T7.1, decision Q35): the pure translation layer
//! between init-pro's first-class `init-pro.io/v1` `Agent` resource and the
//! opencoder engine consumed as path dependencies from `link_repos/opencoder`.
//!
//! This crate bridges [`api::AgentSpec`] onto the opencoder session config
//! types (`opencoder_core::Config` / `opencoder_core::ProviderConfig`) so the
//! scheduler/apiserver never need to know opencoder's shape, and opencoder
//! never needs to know Kubernetes'.
//!
//! Hard scope: translation ONLY — no execution, no I/O, no environment reads.
//! Phase B wires `opencoder_session::run_once` on the target node (see
//! [`session_event_kind`], the only seam kept compiled here so the
//! `opencoder-session` path dependency cannot silently rot out of the
//! closure).

#![forbid(unsafe_code)]

use api::AgentSpec;

/// Failure modes of the `AgentSpec` -> opencoder config bridge.
#[derive(Debug, thiserror::Error)]
pub enum BridgeError {
    #[error("invalid agent spec: {0}")]
    Config(String),
}

/// Builds a [`opencoder_core::ProviderConfig`] carrying opencoder's own serde
/// defaults — most importantly the default `base_url`.
///
/// opencoder's `default_base_url()` is crate-private, so instead of duplicating
/// the value here we instantiate the type through an empty JSON object, which
/// applies the same `#[serde(default = ...)]` defaults opencoder itself uses.
/// No credential material is involved: `api_key` deserializes to `None`.
fn default_provider_config() -> Result<opencoder_core::ProviderConfig, BridgeError> {
    serde_json::from_value(serde_json::json!({})).map_err(|e| {
        BridgeError::Config(format!(
            "opencoder ProviderConfig serde defaults unavailable: {e}"
        ))
    })
}

/// Translates an [`AgentSpec`] payload into an opencoder session [`Config`].
///
/// Mapping (additive onto opencoder's own `Config::default()`):
/// - `spec.model` (when `Some`) overrides `config.model`;
/// - `spec.provider` (when `Some`) registers a named entry in
///   `config.providers` carrying `model: spec.model`, opencoder's DEFAULT
///   `base_url`, and `api_key: None`.
///
/// // Security invariant (Q35): API keys resolve from the environment at
/// // runtime inside opencoder; an AgentSpec carries no secrets, so the bridge
/// // never copies (or fabricates) credential material of any kind.
///
/// Validation: `provider` must be a non-empty name (an empty key would be an
/// unusable `providers[""]` entry) and `model`, when present, must be
/// non-empty. Everything else passes through untouched.
///
/// [`Config`]: opencoder_core::Config
pub fn session_config(spec: &AgentSpec) -> Result<opencoder_core::Config, BridgeError> {
    if let Some(model) = &spec.model {
        if model.trim().is_empty() {
            return Err(BridgeError::Config("model must not be empty".into()));
        }
    }
    let mut config = opencoder_core::Config::default();
    if let Some(model) = &spec.model {
        config.model = model.clone();
    }
    if let Some(provider) = &spec.provider {
        if provider.trim().is_empty() {
            return Err(BridgeError::Config(
                "provider must not be empty when set".into(),
            ));
        }
        let mut entry = default_provider_config()?;
        entry.model = spec.model.clone();
        // Explicit no-credential registration: `api_key` stays `None`; the
        // key is resolved from the environment inside opencoder at runtime.
        entry.api_key = None;
        config.providers.insert(provider.clone(), entry);
    }
    Ok(config)
}

/// The prompt driving the session, verbatim from the spec.
///
/// An empty string means "opencoder default resolution": the node-side Phase B
/// runner decides what an empty prompt means (this crate deliberately does not
/// interpret or reject it).
pub fn prompt_of(spec: &AgentSpec) -> &str {
    &spec.prompt
}

/// The opencoder agent preset to execute (e.g. `"opencoder"`), verbatim from
/// the spec.
///
/// An empty string means "opencoder default resolution": `run_once` falls back
/// to its built-in default agent (`"act"`) when the name does not resolve.
pub fn agent_preset(spec: &AgentSpec) -> &str {
    &spec.agent
}

/// Phase-B runner seam: classifies a session event into a status-report kind.
///
/// Kept as real compiled code (rather than a doc mention) so the
/// `opencoder-session` path dependency stays a verified member of this crate's
/// dependency closure — Phase B's node executor calls
/// `opencoder_session::run_once` with the exact config [`session_config`]
/// builds.
pub fn session_event_kind(event: &opencoder_session::SessionEvent) -> &'static str {
    use opencoder_session::SessionEvent;
    match event {
        SessionEvent::Error(_) => "error",
        SessionEvent::TextDelta(_) | SessionEvent::ReasoningDelta(_) => "text",
        SessionEvent::LlmRoundStart { .. } | SessionEvent::LlmRoundEnd => "llm-round",
        SessionEvent::ToolStart { .. } | SessionEvent::ToolEnd { .. } => "tool",
        _ => "progress",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opencoder_session::SessionEvent;

    /// A spec carrying the full opencoder payload.
    fn spec_with(provider: Option<&str>, model: Option<&str>) -> AgentSpec {
        AgentSpec {
            provider: provider.map(str::to_string),
            model: model.map(str::to_string),
            ..AgentSpec::default()
        }
    }

    /// Security invariant (Q35): no credential material may appear anywhere in
    /// the bridged config — neither in the top-level active provider nor in
    /// any named provider entry.
    fn assert_no_credentials(config: &opencoder_core::Config) {
        assert!(
            config.provider.api_key.is_none(),
            "top-level provider api_key must stay unset"
        );
        assert!(
            config.provider.headers.is_empty(),
            "no credential-bearing headers may be synthesized"
        );
        for (name, entry) in &config.providers {
            assert!(
                entry.api_key.is_none(),
                "provider {name:?} must not carry an api_key"
            );
            assert!(
                entry.headers.is_empty(),
                "provider {name:?} must not carry synthesized headers"
            );
        }
    }

    #[test]
    fn provider_and_model_are_registered() {
        let spec = spec_with(Some("anthropic"), Some("claude-sonnet-4-5"));
        let config = session_config(&spec).expect("bridge succeeds");

        // Model override landed.
        assert_eq!(config.model, "claude-sonnet-4-5");
        // Named provider registered with the model and a default base_url —
        // opencoder's own serde default, not a value hardcoded in this crate.
        let entry = config
            .providers
            .get("anthropic")
            .expect("named provider registered");
        assert_eq!(entry.model.as_deref(), Some("claude-sonnet-4-5"));
        assert_eq!(
            entry.base_url,
            default_provider_config().expect("serde defaults").base_url,
            "base_url must be opencoder's serde default"
        );
        assert_no_credentials(&config);
    }

    #[test]
    fn empty_spec_preserves_opencoder_defaults() {
        let spec = AgentSpec::default();
        let config = session_config(&spec).expect("empty spec is a valid spec");

        // opencoder's own default model is preserved (non-empty, untouched).
        assert_eq!(
            config.model,
            opencoder_core::Config::default().model,
            "model must stay opencoder's default"
        );
        assert!(!config.model.is_empty());
        // providers map unchanged: still exactly opencoder's default (empty).
        assert!(
            config.providers.is_empty(),
            "no providers may be synthesized for an empty spec"
        );
        assert_no_credentials(&config);
    }

    #[test]
    fn provider_without_model_keeps_default_model() {
        let spec = spec_with(Some("openai"), None);
        let config = session_config(&spec).expect("bridge succeeds");

        assert_eq!(
            config.providers.len(),
            1,
            "exactly the named provider is registered"
        );
        let entry = config.providers.get("openai").expect("provider present");
        assert_eq!(entry.model, None, "no model may be fabricated");
        assert_eq!(
            config.model,
            opencoder_core::Config::default().model,
            "config.model must stay the opencoder default"
        );
        assert_no_credentials(&config);
    }

    #[test]
    fn empty_provider_name_is_rejected() {
        let err = session_config(&spec_with(Some(""), Some("m")))
            .expect_err("empty provider name must be rejected");
        assert!(
            err.to_string().contains("invalid agent spec"),
            "error is user-facing: {err}"
        );
        let err = session_config(&spec_with(Some("openai"), Some("")))
            .expect_err("empty model string must be rejected");
        assert!(err.to_string().contains("invalid agent spec"));
    }

    #[test]
    fn session_entry_point_is_importable() {
        // The `opencoder-session` path dependency must stay real (not just
        // declared): reference its re-exported runner event type through the
        // compiled seam function.
        let event = opencoder_session::SessionEvent::LlmRoundEnd;
        assert_eq!(session_event_kind(&event), "llm-round");
        assert_eq!(session_event_kind(&SessionEvent::TextDelta("x".into())), "text");
    }
}
