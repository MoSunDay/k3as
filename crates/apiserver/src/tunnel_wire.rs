//! Reverse relay tunnel WIRE types (TODO **T4.5**, decisions **Q65/Q31**).
//!
//! Split out of [`crate::tunnel_registry`] for the <=400-line new-file
//! budget: this module owns ONLY the JSON wire contract both sides
//! serialize (Q10; camelCase, `bodyB64` keeps Q31's verbatim-bytes
//! contract for non-UTF-8 bodies) — the apiserver emits `RelayRequest`
//! on a parked long-poll and consumes `RelayResponse`; the node-side
//! dialer (`crates/cli/src/tunnel_dialer.rs`) parses the former and
//! builds the latter, so the shapes live exactly once.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::kubelet_hop::KubeletReply;

// ------------------------------------------------------------ wire types ---

/// One relayed kubelet request, handed to the parked long-poll (Q10 JSON;
/// `bodyB64` keeps Q31's verbatim-bytes contract for non-UTF-8 bodies).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RelayRequest {
    pub id: String,
    pub method: String,
    /// Absolute request target on the node-local kubelet
    /// (`/pods/<uid>/containers/<c>/log?...`).
    pub path: String,
    /// Forwarded headers (semantics: `content-type` when a body rides).
    pub headers: HashMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_b64: Option<String>,
}

/// One relayed kubelet response (status + headers + body, base64).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RelayResponse {
    pub status: u16,
    pub headers: HashMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_b64: Option<String>,
}

impl RelayResponse {
    /// Decode into the hop reply type (content-type rides `headers`).
    pub(crate) fn into_reply(self) -> Result<KubeletReply, String> {
        let body = self
            .body_b64
            .as_deref()
            .map(common::b64::decode)
            .transpose()
            .map_err(|e| format!("tunnel response body: {e}"))?
            .unwrap_or_default();
        let content_type = self.headers.get("content-type").cloned();
        Ok(KubeletReply {
            code: self.status,
            content_type,
            body,
        })
    }

    /// Build from a hop reply (encodes the body; content-type rides `headers`).
    pub fn from_reply(reply: &KubeletReply) -> Self {
        let mut headers = HashMap::new();
        if let Some(ct) = &reply.content_type {
            headers.insert("content-type".to_string(), ct.clone());
        }
        RelayResponse {
            status: reply.code,
            headers,
            body_b64: Some(common::b64::encode(&reply.body)),
        }
    }
}
#[cfg(test)]
mod wire_tests {
    use super::*;

    #[test]
    fn response_wire_roundtrip_keeps_bytes() {
        let mut raw = vec![0u8, 159, 146, 150, 255]; // invalid UTF-8 inside
        raw.extend_from_slice(b"log line\n");
        let reply = KubeletReply {
            code: 200,
            content_type: Some("text/plain".into()),
            body: raw.clone(),
        };
        let wire = serde_json::to_value(RelayResponse::from_reply(&reply)).unwrap();
        assert_eq!(wire["status"], 200);
        let back: RelayResponse = serde_json::from_value(wire).unwrap();
        assert_eq!(back.into_reply().unwrap().body, raw);
    }
}
