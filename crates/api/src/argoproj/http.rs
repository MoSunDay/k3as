//! Argo Workflows HTTP task + HTTP auth types (TODO **T7.3 Scope A**, Q30).
//!
//! 1:1 typed port of upstream `http_types.go` (`HTTP`, `HTTPBodySource`,
//! `HTTPHeaders`, `HTTPHeader`, `HTTPHeaderSource`) plus the HTTP auth block
//! from `workflow_types.go` (`Header`, `BasicAuth`, `ClientCertAuth`,
//! `OAuth2Auth`, `OAuth2EndpointParam`, `HTTPAuth`). Field-level SSOT is the
//! Go json tags; wire format is JSON-only (decision **Q10**). Data-only port:
//! the Go helper methods (`ToHeader`, `GetBodyBytes`) are not carried.

use k8s_openapi::api::core::v1 as apiv1;
use serde::{Deserialize, Serialize};

use super::is_false;

/// `HTTPHeaderSource` — sources a header value from a Secret key.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct HTTPHeaderSource {
    /// Secret key selector the header value is read from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_key_ref: Option<apiv1::SecretKeySelector>,
}

/// `HTTPHeaders` — an ordered list of [`HTTPHeader`].
pub type HTTPHeaders = Vec<HTTPHeader>;

/// `HTTPBodySource` — the source of an HTTP request body.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct HTTPBodySource {
    /// Raw body bytes, base64-encoded on the wire (Go `[]byte` json encoding).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes: Option<String>,
}

/// `HTTPHeader` — one name/value (or value-from) request header.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct HTTPHeader {
    /// Header name (always serialized).
    #[serde(default)]
    pub name: String,
    /// Literal header value.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub value: String,
    /// Secret-sourced value; when set, `value` is ignored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value_from: Option<HTTPHeaderSource>,
}

/// `HTTP` — an HTTP request performed as a template.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct HTTP {
    /// HTTP method for the request.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub method: String,
    /// URL of the HTTP request (always serialized).
    #[serde(default)]
    pub url: String,
    /// Optional headers sent with the request.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headers: HTTPHeaders,
    /// Request timeout in seconds (default 30).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_seconds: Option<i64>,
    /// Expression which, when true, marks the request successful.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub success_condition: String,
    /// Literal body content of the request.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub body: String,
    /// Body content supplied as bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_from: Option<HTTPBodySource>,
    /// Skip TLS verification for the HTTP client.
    #[serde(default, skip_serializing_if = "is_false")]
    pub insecure_skip_verify: bool,
}

/// `Header` — a key/value request header used when fetching artifacts over
/// HTTP.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Header {
    /// Header name (always serialized).
    #[serde(default)]
    pub name: String,
    /// Literal header value (always serialized).
    #[serde(default)]
    pub value: String,
}

/// `BasicAuth` — secret selectors for basic authentication.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct BasicAuth {
    /// Secret selector for the username.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username_secret: Option<apiv1::SecretKeySelector>,
    /// Secret selector for the password.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password_secret: Option<apiv1::SecretKeySelector>,
}

/// `ClientCertAuth` — client authentication via certificates.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ClientCertAuth {
    /// Secret selector for the client certificate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_cert_secret: Option<apiv1::SecretKeySelector>,
    /// Secret selector for the client key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_key_secret: Option<apiv1::SecretKeySelector>,
}

/// `OAuth2Auth` — client authentication via OAuth2 tokens.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct OAuth2Auth {
    /// Secret selector for the client ID.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "clientIDSecret"
    )]
    pub client_id_secret: Option<apiv1::SecretKeySelector>,
    /// Secret selector for the client secret.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret_secret: Option<apiv1::SecretKeySelector>,
    /// Secret selector for the token URL.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "tokenURLSecret"
    )]
    pub token_url_secret: Option<apiv1::SecretKeySelector>,
    /// Requested OAuth2 scopes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scopes: Vec<String>,
    /// Extra parameters sent in the token request.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub endpoint_params: Vec<OAuth2EndpointParam>,
}

/// `OAuth2EndpointParam` — one parameter sent in the OAuth request.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct OAuth2EndpointParam {
    /// Parameter key (always serialized).
    #[serde(default)]
    pub key: String,
    /// Literal parameter value.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub value: String,
}

/// `HTTPAuth` — client-authentication payload for HTTP artifact fetches.
///
/// Go embeds the three flavors as non-pointer structs with a no-op
/// `omitempty`, so they are always serialized.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct HTTPAuth {
    /// mTLS client certificate auth.
    #[serde(default)]
    pub client_cert: ClientCertAuth,
    /// OAuth2 token auth.
    #[serde(default)]
    pub oauth2: OAuth2Auth,
    /// Basic auth.
    #[serde(default)]
    pub basic_auth: BasicAuth,
}

/// `HTTPArtifact` — artifact served over plain HTTP(S) (workflow_types.go).
///
/// Required `url`; headers/auth optional; `saveStreamViaFile` omitempty.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct HTTPArtifact {
    /// URL of the artifact (its path component is the storage key).
    #[serde(default)]
    pub url: String,
    /// Extra request headers.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<Header>,
    /// Optional HTTP authentication.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<HTTPAuth>,
    /// If true, save the byte stream to a local file instead of buffering
    /// it in memory (large artifacts).
    #[serde(default, skip_serializing_if = "is_false")]
    pub save_stream_via_file: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_serializes_camel_case_and_omits_empty() {
        let req = HTTP {
            method: "GET".to_string(),
            url: "https://example.com/api".to_string(),
            headers: vec![HTTPHeader {
                name: "X-Token".to_string(),
                value: "abc".to_string(),
                value_from: None,
            }],
            ..Default::default()
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "method": "GET",
                "url": "https://example.com/api",
                "headers": [{"name": "X-Token", "value": "abc"}]
            })
        );
        // Round-trips back into the same value.
        assert_eq!(serde_json::from_value::<HTTP>(json).unwrap(), req);
    }

    #[test]
    fn header_deserializes_from_name_value() {
        let h: Header = serde_json::from_value(
            serde_json::json!({"name": "Authorization", "value": "Bearer x"}),
        )
        .unwrap();
        assert_eq!(h.name, "Authorization");
        assert_eq!(h.value, "Bearer x");
        // Go has no omitempty on either field -> both always serialize.
        assert_eq!(
            serde_json::to_value(&h).unwrap(),
            serde_json::json!({"name": "Authorization", "value": "Bearer x"})
        );
    }
}
