//! Client side of the env projection (TODO **T4.7**): fetches the
//! referenced ConfigMaps/Secrets through the shared kubelet->apiserver
//! [`HttpJson`] client (the same surface the configMap/secret VOLUME
//! projection uses), resolves every desired pod's container env via
//! [`crate::env::resolve_env`], and POSTs upstream-parity
//! `CreateContainerConfigError` events for containers whose create is
//! blocked by a missing required reference. Resolution is CREATE-time
//! only (Q-parity with upstream kubelet): a running container keeps the
//! env it was created with, so events fire only while the container is
//! absent — the retry is simply the next sync cycle once the object
//! appears (no watch linkage).

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

use crate::env::{config_error_event, resolve_env, EnvFromSource, EnvSources, EnvValue, RefKind};
use crate::http::HttpJson;
use crate::objects::{ContainerSpec, PodView};
use crate::sync::Snapshot;

/// One container's resolution outcome for a sync cycle: the env pairs to
/// bake into the CRI config, or the error that blocks the create.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvOutcome {
    pub envs: Vec<(String, String)>,
    pub error: Option<String>,
}

/// Resolved env per pod key -> container name. Only containers WITH env
/// or envFrom references have entries; plan treats absent as "no env".
pub type EnvTable = BTreeMap<String, BTreeMap<String, EnvOutcome>>;

/// True when the container declares at least one env/envFrom reference
/// (i.e. resolution has work to do and a create-time say).
fn has_refs(c: &ContainerSpec) -> bool {
    !c.env.is_empty() || !c.env_from.is_empty()
}

/// Collect the distinct object names one container references.
fn collect_refs(c: &ContainerSpec, cms: &mut BTreeSet<String>, secs: &mut BTreeSet<String>) {
    for src in &c.env_from {
        match src {
            EnvFromSource::ConfigMap { name, .. } => {
                cms.insert(name.clone());
            }
            EnvFromSource::Secret { name, .. } => {
                secs.insert(name.clone());
            }
        }
    }
    for var in &c.env {
        match &var.value {
            EnvValue::Literal(_) => {}
            EnvValue::ConfigMapKeyRef { name, .. } => {
                cms.insert(name.clone());
            }
            EnvValue::SecretKeyRef { name, .. } => {
                secs.insert(name.clone());
            }
        }
    }
}

/// Fetch one configMap/secret: `Ok(Some(body))` = present, `Ok(None)` =
/// definitive 404 (optional semantics apply), `Err` = the fetch itself
/// failed (any non-404 HTTP status or transport error — resolution must
/// error even for `optional: true` references, upstream parity).
async fn fetch_one(
    client: &HttpJson,
    namespace: &str,
    resource: &str,
    name: &str,
) -> Result<Option<Value>, String> {
    let path = format!("/api/v1/namespaces/{namespace}/{resource}/{name}");
    match client.get_json(&path).await {
        Ok((200, body)) => Ok(Some(body)),
        Ok((404, _)) => Ok(None),
        Ok((code, _)) => Err(format!("GET {path} -> HTTP {code}")),
        Err(e) => Err(format!("GET {path}: {e}")),
    }
}

/// Resolve env for every non-deleted desired pod. Pods whose containers
/// declare no references are skipped WITHOUT any client round-trip.
pub async fn resolve_table(client: &HttpJson, wanted: &BTreeMap<String, PodView>) -> EnvTable {
    let mut table = EnvTable::new();
    for (key, view) in wanted {
        if view.deleted || !view.containers.iter().any(has_refs) {
            continue;
        }
        let mut cms: BTreeSet<String> = BTreeSet::new();
        let mut secs: BTreeSet<String> = BTreeSet::new();
        for c in &view.containers {
            collect_refs(c, &mut cms, &mut secs);
        }
        let mut sources = EnvSources::default();
        for name in &cms {
            match fetch_one(client, &view.namespace, "configmaps", name).await {
                Ok(body) => {
                    sources.configmaps.insert(name.clone(), body);
                }
                Err(e) => {
                    sources
                        .fetch_errors
                        .insert(format!("{}/{}", RefKind::ConfigMap.word(), name), e);
                }
            }
        }
        for name in &secs {
            match fetch_one(client, &view.namespace, "secrets", name).await {
                Ok(body) => {
                    sources.secrets.insert(name.clone(), body);
                }
                Err(e) => {
                    sources
                        .fetch_errors
                        .insert(format!("{}/{}", RefKind::Secret.word(), name), e);
                }
            }
        }
        let per: BTreeMap<String, EnvOutcome> = view
            .containers
            .iter()
            .filter(|c| has_refs(c))
            .map(|c| {
                let outcome = match resolve_env(&c.env, &c.env_from, &sources) {
                    Ok(envs) => EnvOutcome { envs, error: None },
                    Err(error) => EnvOutcome {
                        envs: Vec::new(),
                        error: Some(error),
                    },
                };
                (c.name.clone(), outcome)
            })
            .collect();
        table.insert(key.clone(), per);
    }
    table
}

/// POST `CreateContainerConfigError` events for blocked containers that
/// do NOT exist yet (an already-created container keeps its env — events
/// would be premature restart reporting). `last` dedups by pod/container
/// and exact reason so a steady missing object emits ONE event, and it
/// prunes entries for gone pods.
pub async fn emit_config_error_events(
    client: &HttpJson,
    node_name: &str,
    wanted: &BTreeMap<String, PodView>,
    snap: &Snapshot,
    table: &EnvTable,
    last: &mut BTreeMap<String, String>,
    now: &str,
) {
    let mut live: BTreeSet<String> = BTreeSet::new();
    for (key, view) in wanted {
        if view.deleted {
            continue;
        }
        let Some(per_container) = table.get(key) else {
            continue;
        };
        for (container, outcome) in per_container {
            let Some(reason) = &outcome.error else {
                continue;
            };
            let dedup = format!("{key}/{container}");
            live.insert(dedup.clone());
            if last.get(&dedup).is_some_and(|r| r == reason) {
                continue; // already reported this exact reason
            }
            if container_exists(snap, key, container) {
                continue; // created already: env frozen at create time
            }
            let event = config_error_event(
                &view.namespace,
                &view.name,
                &view.uid,
                container,
                reason,
                node_name,
                now,
            );
            let path = format!("/api/v1/namespaces/{}/events", view.namespace);
            match client.post_json(&path, &event).await {
                Ok((201, _)) => {
                    last.insert(dedup, reason.clone());
                }
                Ok((code, body)) => tracing::debug!(
                    target: "init-pro",
                    code = code, %body,
                    "kubelet: env error event POST rejected"
                ),
                Err(e) => tracing::debug!(
                    target: "init-pro",
                    error = %e,
                    "kubelet: env error event POST failed"
                ),
            }
        }
    }
    last.retain(|k, _| live.contains(k));
}

/// True when a container with this pod key + name exists in the snapshot.
fn container_exists(snap: &Snapshot, pod_key: &str, container: &str) -> bool {
    snap.containers.iter().any(|c| {
        c.name == container
            && crate::sync::container_pod_key(c, &snap.sandboxes).as_deref() == Some(pod_key)
    })
}
