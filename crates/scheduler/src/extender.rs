//! Scheduler extender seam (TODO **T3.2**, decision **Q3**/Q23).
//!
//! Upstream-compatible HTTP JSON extenders (kube-scheduler extender wire
//! protocol, HTTP-only per **Q10**):
//!  - filter: `POST {url}/{filterVerb}` with `{"pod": ..., "nodes":
//!    {"Items": [...]}}` (`nodeCacheCapable=false`); the extender answers
//!    `{"NodeNames": [...]}` or `{"Nodes": {"Items": [...]}}` (or legacy
//!    `{"Nodes": [...]}`) — the feasible subset.
//!  - prioritize: `POST {url}/{prioritizeVerb}` with the same request; the
//!    answer is `[{"host": ..., "score": ...}, ...]`, weighted by `weight`
//!    and added to the local score.
//!  - failure semantics: `ignorable=true` extenders degrade to no-ops with a
//!    warning; otherwise one failed extender fails the whole attempt (the
//!    pod is retried, never hot-looped).

use std::collections::HashMap;

use serde::Deserialize;
use serde_json::{json, Value};
use tokio::time::Duration;

use crate::http::HttpClient;
use crate::plugin::NodeInfo;

fn default_filter_verb() -> String {
    "filter".into()
}
fn default_prioritize_verb() -> String {
    "prioritize".into()
}
fn default_weight() -> i64 {
    1
}
fn default_timeout_ms() -> u64 {
    10_000
}

/// One extender endpoint (upstream `ExtenderConfig` subset).
#[derive(Debug, Clone, Deserialize)]
pub struct ExtenderConfig {
    #[serde(rename = "urlPrefix", alias = "url")]
    pub url_prefix: String,
    #[serde(default = "default_filter_verb")]
    pub filter_verb: String,
    #[serde(default = "default_prioritize_verb")]
    pub prioritize_verb: String,
    #[serde(default = "default_weight")]
    pub weight: i64,
    #[serde(default)]
    pub ignorable: bool,
    #[serde(default)]
    pub node_cache_capable: bool,
    #[serde(default = "default_timeout_ms", rename = "httpTimeoutMs")]
    pub http_timeout_ms: u64,
}

/// The configured extender set (usually zero or one entries in v1).
pub struct ExtenderSet {
    extenders: Vec<(ExtenderConfig, HttpClient)>,
}

impl ExtenderSet {
    pub fn from_configs(configs: &[ExtenderConfig]) -> Result<Self, String> {
        let extenders = configs
            .iter()
            .map(|c| Ok((c.clone(), HttpClient::parse(&c.url_prefix)?)))
            .collect::<Result<Vec<_>, String>>()?;
        Ok(ExtenderSet { extenders })
    }

    pub fn is_empty(&self) -> bool {
        self.extenders.is_empty()
    }

    /// Extender filter phase: start from `feasible` (already locally
    /// filtered), return the subset every extender still accepts. A
    /// non-ignorable extender failure fails the whole attempt (`Err`).
    pub async fn filter(&self, pod: &Value, feasible: &[&NodeInfo]) -> Result<Vec<String>, String> {
        let mut names: Vec<String> = feasible
            .iter()
            .filter_map(|i| {
                i.node
                    .pointer("/metadata/name")
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
            })
            .collect();
        for (cfg, http) in &self.extenders {
            if cfg.filter_verb.is_empty() {
                continue;
            }
            let nodes = nodes_payload(feasible, cfg.node_cache_capable);
            let body = json!({"pod": pod, "nodes": nodes});
            match call(http, &cfg.filter_verb, &body, cfg.http_timeout_ms).await {
                Ok(resp) => {
                    let keep = parse_filter_response(&resp, &names);
                    match keep {
                        Some(kept) => {
                            names.retain(|n| kept.contains(n));
                        }
                        None => {
                            return Err(format!(
                                "extender {}: unparseable filter response",
                                cfg.url_prefix
                            ))
                        }
                    }
                }
                Err(e) if cfg.ignorable => {
                    tracing::warn!(target: "init-pro", url = %cfg.url_prefix, error = %e, "ignorable extender filter failed; skipping");
                }
                Err(e) => return Err(format!("extender {}: {e}", cfg.url_prefix)),
            }
            if names.is_empty() {
                return Ok(names);
            }
        }
        Ok(names)
    }

    /// Extender prioritize phase: host -> weighted score delta.
    pub async fn prioritize(
        &self,
        pod: &Value,
        feasible: &[&NodeInfo],
    ) -> Result<HashMap<String, i64>, String> {
        let mut out: HashMap<String, i64> = HashMap::new();
        for (cfg, http) in &self.extenders {
            if cfg.prioritize_verb.is_empty() {
                continue;
            }
            let nodes = nodes_payload(feasible, cfg.node_cache_capable);
            let body = json!({"pod": pod, "nodes": nodes});
            match call(http, &cfg.prioritize_verb, &body, cfg.http_timeout_ms).await {
                Ok(resp) => {
                    if let Some(list) = resp.as_array() {
                        for item in list {
                            let host = item.get("host").and_then(|h| h.as_str());
                            let score = item.get("score").and_then(|s| s.as_i64());
                            if let (Some(host), Some(score)) = (host, score) {
                                *out.entry(host.to_string()).or_insert(0) += score * cfg.weight;
                            }
                        }
                    }
                }
                Err(e) if cfg.ignorable => {
                    tracing::warn!(target: "init-pro", url = %cfg.url_prefix, error = %e, "ignorable extender prioritize failed; skipping");
                }
                Err(e) => return Err(format!("extender {}: {e}", cfg.url_prefix)),
            }
        }
        Ok(out)
    }
}

/// The `nodes` field: full objects (`nodeCacheCapable=false`, our default) or
/// names only (`nodeCacheCapable=true`).
fn nodes_payload(feasible: &[&NodeInfo], node_cache_capable: bool) -> Value {
    if node_cache_capable {
        let names: Vec<&str> = feasible
            .iter()
            .filter_map(|i| i.node.pointer("/metadata/name").and_then(|v| v.as_str()))
            .collect();
        json!({"NodeNames": names, "Items": []})
    } else {
        let items: Vec<&Value> = feasible.iter().map(|i| i.node.as_ref()).collect();
        json!({"Items": items})
    }
}

/// One POST with the extender's timeout. Non-2xx is an error.
async fn call(
    http: &HttpClient,
    verb: &str,
    body: &Value,
    timeout_ms: u64,
) -> Result<Value, String> {
    let path = format!("/{verb}");
    let fut = http.post_json(&path, body);
    match tokio::time::timeout(Duration::from_millis(timeout_ms.max(1)), fut).await {
        Ok(Ok((200..=299, v))) => Ok(v),
        Ok(Ok((code, _))) => Err(format!("HTTP {code}")),
        Ok(Err(e)) => Err(e),
        Err(_) => Err("extender call timed out".into()),
    }
}

/// Interpret a filter response: `NodeNames` or `Nodes.Items[].metadata.name`
/// (or a legacy `Nodes` array). `None` = unparseable.
fn parse_filter_response(resp: &Value, current: &[String]) -> Option<Vec<String>> {
    let _ = current;
    if let Some(names) = resp.get("NodeNames").and_then(|v| v.as_array()) {
        return Some(
            names
                .iter()
                .filter_map(|n| n.as_str().map(str::to_string))
                .collect(),
        );
    }
    for path in ["/Nodes/Items", "/nodes/Items"] {
        if let Some(items) = resp.pointer(path).and_then(|v| v.as_array()) {
            return Some(
                items
                    .iter()
                    .filter_map(|n| {
                        n.pointer("/metadata/name")
                            .or_else(|| n.get("name"))
                            .and_then(|v| v.as_str())
                            .map(str::to_string)
                    })
                    .collect(),
            );
        }
    }
    // Legacy: a bare array of node objects.
    if let Some(items) = resp.get("Nodes").and_then(|v| v.as_array()) {
        return Some(
            items
                .iter()
                .filter_map(|n| {
                    n.pointer("/metadata/name")
                        .or_else(|| n.get("name"))
                        .and_then(|v| v.as_str())
                        .map(str::to_string)
                })
                .collect(),
        );
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn config_defaults_match_upstream_verbs() {
        let cfg: ExtenderConfig =
            serde_json::from_str(r#"{"url": "http://127.0.0.1:8889/ext1"}"#).unwrap();
        assert_eq!(cfg.filter_verb, "filter");
        assert_eq!(cfg.prioritize_verb, "prioritize");
        assert_eq!(cfg.weight, 1);
        assert!(!cfg.ignorable);
        assert!(!cfg.node_cache_capable);
        assert_eq!(cfg.http_timeout_ms, 10_000);
    }

    #[test]
    fn filter_response_shapes_all_parse() {
        let a = json!({"NodeNames": ["a", "b"]});
        assert_eq!(
            parse_filter_response(&a, &[]),
            Some(vec!["a".into(), "b".into()])
        );
        let b = json!({"Nodes": {"Items": [
            {"metadata": {"name": "a"}}, {"metadata": {"name": "b"}}
        ]}});
        assert_eq!(
            parse_filter_response(&b, &[]),
            Some(vec!["a".into(), "b".into()])
        );
        let legacy = json!({"Nodes": [{"name": "a"}]});
        assert_eq!(parse_filter_response(&legacy, &[]), Some(vec!["a".into()]));
        assert_eq!(parse_filter_response(&json!({}), &[]), None);
    }

    #[test]
    fn nodes_payload_switches_on_node_cache_capable() {
        let node = Arc::new(json!({"metadata": {"name": "a"}}));
        let info = NodeInfo { node, pods: vec![] };
        let full = nodes_payload(&[&info], false);
        assert!(full.pointer("/Items/0/metadata/name").is_some());
        let names = nodes_payload(&[&info], true);
        assert_eq!(names.pointer("/NodeNames/0"), Some(&json!("a")));
    }
}
