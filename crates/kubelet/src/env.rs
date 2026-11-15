//! Container env/envFrom projection (TODO **T4.7**). Pure spec model +
//! resolver: `spec.containers[].env` (literal `value`, plus the
//! single-key `valueFrom.configMapKeyRef`/`secretKeyRef` forms — they
//! share the same fetch+resolve machinery) and
//! `spec.containers[].envFrom` (whole-object ConfigMap/Secret refs).
//!
//! k8s-parity semantics encoded here (verified against upstream kubelet
//! `makeEnvironmentVariables`): resolution happens at container CREATE
//! time only (env is never hot-updated; no watch linkage), envFrom
//! entries are appended FIRST (each source's keys in sorted order for
//! determinism), then explicit env entries in spec order, and duplicate
//! names resolve LAST-WRITE-WINS — an explicit env entry overrides an
//! envFrom key of the same name. `optional: true` on a reference makes a
//! MISSING object contribute nothing; `optional` absent/false plus a
//! missing required object fails the create (upstream surfaces this as
//! the `CreateContainerConfigError` event, built by
//! [`config_error_event`]). Fetch errors (non-404) fail the resolution
//! regardless of `optional` — upstream cannot honor optionality against
//! an unknown state either.
//!
//! Data values follow the configMap/secret VOLUME convention exactly
//! ([`crate::volumes`]): only the object's `/data` map counts, only
//! STRING values are used (the init-pro store keeps plain JSON strings,
//! never base64), and non-string rows contribute nothing.
//!
//! Deferred (out of T4.7 scope): Downward API (`fieldRef`/
//! `resourceFieldRef`), projected env sources, `envFrom[].prefix`.

use std::collections::BTreeMap;

use serde_json::{json, Value};

/// One `spec.containers[].env[]` entry: `name` plus either a literal
/// `value` or a single-key `valueFrom` reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvVar {
    pub name: String,
    pub value: EnvValue,
}

/// The value side of one env entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvValue {
    Literal(String),
    ConfigMapKeyRef {
        name: String,
        key: String,
        optional: bool,
    },
    SecretKeyRef {
        name: String,
        key: String,
        optional: bool,
    },
}

/// One `spec.containers[].envFrom[]` source (whole-object reference).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvFromSource {
    ConfigMap { name: String, optional: bool },
    Secret { name: String, optional: bool },
}

/// `spec.containers[].env[]` as [`EnvVar`]s. Malformed rows — no `name`,
/// no `value`/recognized `valueFrom` — are skipped, never fatal (same
/// tolerance as the other `pod_containers` field parsers).
pub fn parse_env(c: &Value) -> Vec<EnvVar> {
    let Some(items) = c.pointer("/env").and_then(Value::as_array) else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|e| {
            let name = e.pointer("/name").and_then(Value::as_str)?;
            if name.is_empty() {
                return None;
            }
            Some(EnvVar {
                name: name.to_string(),
                value: env_value(e)?,
            })
        })
        .collect()
}

/// The value side of one env entry: literal `value` first, then the
/// single-key `valueFrom` refs; `None` when the row has neither (or an
/// unsupported `valueFrom` like `fieldRef` — Downward API is deferred).
fn env_value(e: &Value) -> Option<EnvValue> {
    if let Some(v) = e.pointer("/value").and_then(Value::as_str) {
        return Some(EnvValue::Literal(v.to_string()));
    }
    if let Some((name, key, optional)) = key_ref(e, "configMapKeyRef") {
        return Some(EnvValue::ConfigMapKeyRef {
            name,
            key,
            optional,
        });
    }
    if let Some((name, key, optional)) = key_ref(e, "secretKeyRef") {
        return Some(EnvValue::SecretKeyRef {
            name,
            key,
            optional,
        });
    }
    None
}

/// One `...valueFrom.<field>` single-key reference as (name, key,
/// optional). `optional` defaults to false (k8s).
fn key_ref(e: &Value, field: &str) -> Option<(String, String, bool)> {
    let r = e.pointer(&format!("/valueFrom/{field}"))?;
    let name = r.pointer("/name").and_then(Value::as_str)?;
    let key = r.pointer("/key").and_then(Value::as_str)?;
    if name.is_empty() || key.is_empty() {
        return None;
    }
    Some((
        name.to_string(),
        key.to_string(),
        r.pointer("/optional")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    ))
}

/// `spec.containers[].envFrom[]` as [`EnvFromSource`]s; rows without a
/// usable ref are skipped, never fatal. `optional` defaults to false.
pub fn parse_env_from(c: &Value) -> Vec<EnvFromSource> {
    let Some(items) = c.pointer("/envFrom").and_then(Value::as_array) else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|e| {
            let r = |field: &str| {
                let p = e
                    .pointer(&format!("/{field}/name"))
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())?;
                Some((
                    p.to_string(),
                    e.pointer(&format!("/{field}/optional"))
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                ))
            };
            if let Some((name, optional)) = r("configMapRef") {
                Some(EnvFromSource::ConfigMap { name, optional })
            } else if let Some((name, optional)) = r("secretRef") {
                Some(EnvFromSource::Secret { name, optional })
            } else {
                None
            }
        })
        .collect()
}

/// The ConfigMap/Secret JSON bodies one resolution needs, by object name.
/// `None` means the apiserver answered a definitive 404 (the object is
/// missing — `optional` semantics apply); an entry in `fetch_errors`
/// means the fetch itself failed (non-404/transport — resolution errors
/// regardless of `optional`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EnvSources {
    pub configmaps: BTreeMap<String, Option<Value>>,
    pub secrets: BTreeMap<String, Option<Value>>,
    pub fetch_errors: BTreeMap<String, String>,
}

/// Which object kind a reference names (messages + map lookup key).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefKind {
    ConfigMap,
    Secret,
}

impl RefKind {
    /// The k8s kind word used in upstream error strings.
    pub fn word(self) -> &'static str {
        match self {
            RefKind::ConfigMap => "configmap",
            RefKind::Secret => "secret",
        }
    }
}

impl EnvSources {
    /// The fetch error recorded for one object, if any.
    pub fn fetch_error(&self, kind: RefKind, name: &str) -> Option<String> {
        self.fetch_errors
            .get(&format!("{}/{name}", kind.word()))
            .cloned()
    }

    /// The object's `/data` map when it exists (string values only, the
    /// volumes.rs convention). `Ok(None)` = missing-and-optional-OK.
    fn data(
        &self,
        kind: RefKind,
        name: &str,
        optional: bool,
    ) -> Result<Option<BTreeMap<String, String>>, String> {
        if let Some(err) = self.fetch_error(kind, name) {
            return Err(err);
        }
        let obj = match kind {
            RefKind::ConfigMap => self.configmaps.get(name),
            RefKind::Secret => self.secrets.get(name),
        };
        match obj {
            None | Some(None) => {
                if optional {
                    Ok(None)
                } else {
                    Err(format!("{} \"{name}\" not found", kind.word()))
                }
            }
            Some(Some(body)) => Ok(Some(data_map(body))),
        }
    }
}

/// `/data` as a sorted string map — the exact convention of the
/// configMap/secret volume projection (volumes.rs `data_map`): a missing
/// `data` field or non-string values contribute nothing.
fn data_map(body: &Value) -> BTreeMap<String, String> {
    body.pointer("/data")
        .and_then(Value::as_object)
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default()
}

/// Resolve one container's env. Ordering/precedence per the module docs:
/// envFrom sources first (spec order, keys sorted within a source), then
/// explicit env entries; duplicate names keep the FIRST position and the
/// LAST value (explicit env overrides envFrom).
pub fn resolve_env(
    env: &[EnvVar],
    env_from: &[EnvFromSource],
    sources: &EnvSources,
) -> Result<Vec<(String, String)>, String> {
    let mut out: Vec<(String, String)> = Vec::new();
    for src in env_from {
        let (kind, name, optional) = match src {
            EnvFromSource::ConfigMap { name, optional } => (RefKind::ConfigMap, name, *optional),
            EnvFromSource::Secret { name, optional } => (RefKind::Secret, name, *optional),
        };
        let Some(data) = sources.data(kind, name, optional)? else {
            continue; // missing but optional: contributes nothing
        };
        for (k, v) in data {
            if !is_env_name(&k) {
                continue; // a key that cannot name an env var is skipped
            }
            upsert(&mut out, k, v);
        }
    }
    for var in env {
        let value = match &var.value {
            EnvValue::Literal(v) => Some(v.clone()),
            EnvValue::ConfigMapKeyRef {
                name,
                key,
                optional,
            } => key_value(sources, RefKind::ConfigMap, name, key, *optional)?,
            EnvValue::SecretKeyRef {
                name,
                key,
                optional,
            } => key_value(sources, RefKind::Secret, name, key, *optional)?,
        };
        if let Some(v) = value {
            upsert(&mut out, var.name.clone(), v);
        }
    }
    Ok(out)
}

/// One single-key `*KeyRef` value; `Ok(None)` = absent-and-optional.
fn key_value(
    sources: &EnvSources,
    kind: RefKind,
    name: &str,
    key: &str,
    optional: bool,
) -> Result<Option<String>, String> {
    let Some(data) = sources.data(kind, name, optional)? else {
        return Ok(None);
    };
    match data.get(key) {
        Some(v) => Ok(Some(v.clone())),
        None if optional => Ok(None),
        None => Err(format!(
            "key \"{key}\" not found in {} \"{name}\"",
            kind.word()
        )),
    }
}

/// Insert/overwrite by name: keeps the first position, takes the last
/// value (k8s precedence — explicit env overrides envFrom).
fn upsert(out: &mut Vec<(String, String)>, name: String, value: String) {
    if let Some(slot) = out.iter_mut().find(|(n, _)| *n == name) {
        slot.1 = value;
    } else {
        out.push((name, value));
    }
}

/// Env-var-name guard for envFrom keys: non-empty, no `=` and no NUL
/// (anything the runtime could not round-trip as one var).
fn is_env_name(k: &str) -> bool {
    !k.is_empty() && !k.contains('=') && !k.contains('\0')
}

/// A core/v1 Warning event for a container create blocked by env
/// resolution (upstream reason `CreateContainerConfigError`), ready to
/// POST to the apiserver events REST; `generateName` lets the apiserver
/// suffix a unique name (no client-side event-recorder infra invented).
pub fn config_error_event(
    namespace: &str,
    pod: &str,
    uid: &str,
    container: &str,
    message: &str,
    node: &str,
    now: &str,
) -> Value {
    json!({
        "apiVersion": "v1",
        "kind": "Event",
        "metadata": {
            "generateName": format!("{pod}.{container}."),
            "namespace": namespace,
        },
        "involvedObject": {
            "kind": "Pod",
            "apiVersion": "v1",
            "namespace": namespace,
            "name": pod,
            "uid": uid,
        },
        "reason": "CreateContainerConfigError",
        "message": message,
        "type": "Warning",
        "count": 1,
        "firstTimestamp": now,
        "lastTimestamp": now,
        "source": {"component": "kubelet", "host": node},
    })
}
