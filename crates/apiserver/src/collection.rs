//! REST collection handlers: create / list / watch (TODO **T1.2b**).
//!
//! Routes (core group `""` + grouped):
//!  - `POST /<collection>`         -> create
//!  - `GET  /<collection>`         -> list (`?limit=&continue=`)
//!  - `GET  /<collection>?watch=1` -> chunked `application/json` watch stream
//!
//! Scope rules (upstream parity): a namespaced-path targets only namespaced
//! resources; the cluster-path collection lists across all namespaces for
//! namespaced resources AND serves cluster-scoped resources.

use axum::body::{Body, Bytes};
use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use storage::{StoredEntry, WatchEvent};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use crate::error::{storage_error, ApiError};
use crate::service;
use crate::state::{
    cluster_key, collection_prefix, namespaced_key, resolve, set_namespace, set_resource_version,
    set_type_meta, AppState, Loc, Resolved,
};

/// `GET /<collection>?watch=1&resourceVersion=&limit=&continue=`.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct ListParams {
    /// `watch=1` (or `true`) selects the watch stream over the list snapshot.
    #[serde(default)]
    pub watch: Option<String>,
    /// Page size (0/absent = return all).
    #[serde(default)]
    pub limit: Option<i64>,
    /// Opaque cursor returned by a previous page (the next storage key).
    #[serde(default, rename = "continue")]
    pub continue_token: Option<String>,
    /// Watch start point (Q10 wire name `resourceVersion`): absent = live
    /// only, `0` = replay retained history, `N` = events after N.
    #[serde(default, rename = "resourceVersion")]
    pub resource_version: Option<String>,
}

impl ListParams {
    fn is_watch(&self) -> bool {
        matches!(
            self.watch.as_deref(),
            Some("1") | Some("true") | Some("True")
        )
    }
}

// ---------------------------------------------------------------------------
// Shared logic
// ---------------------------------------------------------------------------

pub(crate) async fn do_create(st: &AppState, loc: &Loc, mut body: Value) -> Response {
    let res = match resolve(&st.registry, &loc.group, &loc.version, &loc.resource) {
        Some(r) => r,
        None => return ApiError::NotFoundResource.into_response(),
    };
    // Create requires an exact scope/path match: a namespaced resource must be
    // created via the namespaced path; a cluster resource via the cluster path.
    if res.scope.is_namespaced() != loc.namespace.is_some() {
        return ApiError::NotFoundResource.into_response();
    }
    let name = match super::state::object_name(&body) {
        Some(n) => n,
        None => {
            return ApiError::Invalid {
                kind: res.kind.clone(),
                message: "a name is required (metadata.name)".into(),
            }
            .into_response();
        }
    };
    let namespace = loc
        .namespace
        .clone()
        .or_else(|| super::state::object_namespace(&body))
        .unwrap_or_default();
    let key = if res.scope.is_namespaced() {
        namespaced_key(&loc.group, &loc.resource, &namespace, &name)
    } else {
        cluster_key(&loc.group, &loc.resource, &name)
    };
    set_type_meta(&mut body, &res.api_version, &res.kind);
    if res.scope.is_namespaced() {
        set_namespace(&mut body, &namespace);
    }
    // Upstream apiserver parity (T3.1b, Q20): every Namespace is created with
    // the `kubernetes` finalizer so deletion is gated on the namespace
    // controller draining all namespaced content first.
    if res.kind == "Namespace" {
        ensure_namespace_finalizer(&mut body);
    }
    // Sprint 18 / S3: Service defaulting — spec.type defaults to ClusterIP;
    // NodePort/LoadBalancer services get nodePorts allocated (30000-32767).
    if res.kind == "Service" {
        if let Err(service::ServiceError::Invalid(message)) =
            service::prepare(&st.store, &mut body).await
        {
            return ApiError::Invalid {
                kind: res.kind.clone(),
                message,
            }
            .into_response();
        }
    }
    match st.store.create(&key, body).await {
        Ok(entry) => {
            let entry = if res.kind == "Service" {
                service::heal_nodeport_collision(&st.store, &key, entry).await
            } else {
                entry
            };
            let mut out = entry.value;
            set_resource_version(&mut out, entry.mod_revision);
            (StatusCode::CREATED, Json(out)).into_response()
        }
        Err(e) => storage_error(e, &res.kind, &name).into_response(),
    }
}

pub(crate) async fn do_list(st: &AppState, loc: &Loc, params: &ListParams) -> Response {
    let res = match resolve(&st.registry, &loc.group, &loc.version, &loc.resource) {
        Some(r) => r,
        None => return ApiError::NotFoundResource.into_response(),
    };
    // The namespaced path only applies to namespaced resources.
    if loc.namespace.is_some() && !res.scope.is_namespaced() {
        return ApiError::NotFoundResource.into_response();
    }
    let prefix = collection_prefix(&loc.group, &loc.resource, loc.namespace.as_deref());
    if params.is_watch() {
        return do_watch(st, &res, &prefix, params).await;
    }
    let entries = match st.store.list(&prefix).await {
        Ok(v) => v,
        Err(e) => return storage_error(e, &res.kind, "").into_response(),
    };
    let rev = st.store.current_revision().await.unwrap_or(0);
    let (page, continue_token) = paginate(&entries, params);
    let items: Vec<Value> = page
        .iter()
        .map(|e| {
            let mut v = e.value.clone();
            set_resource_version(&mut v, e.mod_revision);
            v
        })
        .collect();
    let body = json!({
        "kind": res.list_kind,
        "apiVersion": res.api_version,
        "metadata": { "resourceVersion": rev.to_string(), "continue": continue_token },
        "items": items,
    });
    (StatusCode::OK, Json(body)).into_response()
}

/// Set `metadata.finalizers = ["kubernetes"]` when absent/empty (namespace
/// creation; upstream injects this spec finalizer on every Namespace).
fn ensure_namespace_finalizer(body: &mut Value) {
    let already_set = body
        .pointer("/metadata/finalizers")
        .and_then(Value::as_array)
        .is_some_and(|a| !a.is_empty());
    if already_set {
        return;
    }
    let Some(obj) = body.as_object_mut() else {
        return;
    };
    let meta = obj
        .entry("metadata")
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    if let Some(m) = meta.as_object_mut() {
        m.insert("finalizers".into(), json!(["kubernetes"]));
    }
}

/// Key-cursor pagination over a list sorted by storage key.
fn paginate<'a>(entries: &'a [StoredEntry], params: &ListParams) -> (&'a [StoredEntry], String) {
    let start = match &params.continue_token {
        Some(c) if !c.is_empty() => entries
            .iter()
            .position(|e| e.key.as_str() >= c.as_str())
            .unwrap_or(entries.len()),
        _ => 0,
    };
    let limit = params.limit.unwrap_or(0);
    let end = if limit > 0 {
        (start + limit as usize).min(entries.len())
    } else {
        entries.len()
    };
    let cont = if end < entries.len() {
        entries[end].key.clone()
    } else {
        String::new()
    };
    (&entries[start..end], cont)
}

async fn do_watch(
    st: &AppState,
    res: &Resolved,
    prefix: &storage::KeyPrefix,
    params: &ListParams,
) -> Response {
    // Kubernetes watch semantics: `resourceVersion=N` = "Start at N" (events
    // AFTER N) and `0` = "start at any retained version". The storage trait
    // takes etcd's inclusive `start_revision`, hence the +1 / 0->1 mapping.
    let start_rev = match params.resource_version.as_deref() {
        None | Some("") => None,
        Some(s) => match s.parse::<u64>() {
            Ok(0) => Some(1),
            Ok(n) => Some(n + 1),
            Err(_) => None,
        },
    };
    let watch = match st.store.watch(prefix, start_rev).await {
        Ok(w) => w,
        Err(e) => return storage_error(e, &res.kind, "").into_response(),
    };
    let api_version = res.api_version.clone();
    let kind = res.kind.clone();
    let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(32);
    tokio::spawn(async move {
        let mut w = watch;
        while let Some(ev) = w.recv().await {
            let Some(line) = watch_event_line(&ev, &api_version, &kind) else {
                continue;
            };
            let mut bytes = line.into_bytes();
            bytes.push(b'\n');
            if tx.send(Ok(Bytes::from(bytes))).await.is_err() {
                break; // client disconnected
            }
        }
    });
    let stream = ReceiverStream::new(rx);
    match axum::response::Response::builder()
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::CACHE_CONTROL, "no-cache, private")
        .body(Body::from_stream(stream))
    {
        Ok(r) => r,
        Err(e) => ApiError::Internal(e.to_string()).into_response(),
    }
}

/// Render one watch event as a single JSON line (`{"type","object"}`).
fn watch_event_line(ev: &WatchEvent, api_version: &str, kind: &str) -> Option<String> {
    match ev {
        WatchEvent::Put(e) => {
            let typ = if e.create_revision == e.mod_revision {
                "ADDED"
            } else {
                "MODIFIED"
            };
            let mut obj = e.value.clone();
            set_resource_version(&mut obj, e.mod_revision);
            Some(serde_json::to_string(&json!({ "type": typ, "object": obj })).unwrap_or_default())
        }
        WatchEvent::Delete {
            key,
            mod_revision,
            prev,
        } => {
            // Upstream `DELETED` events carry the object's final state; the
            // deletion revision is stamped as its resourceVersion. Backends
            // that do not retain the previous value degrade to a minimal
            // stub (name is the last path segment).
            let mut obj = match prev {
                Some(e) => e.value.clone(),
                None => json!({
                    "apiVersion": api_version,
                    "kind": kind,
                    "metadata": { "name": key.rsplit('/').next().unwrap_or("") },
                }),
            };
            set_resource_version(&mut obj, *mod_revision);
            Some(
                serde_json::to_string(&json!({ "type": "DELETED", "object": obj }))
                    .unwrap_or_default(),
            )
        }
    }
}

// ---------------------------------------------------------------------------
// axum wrappers (core `""` group + grouped groups)
// ---------------------------------------------------------------------------

// --- core group: /api/v1/<resource> + /api/v1/namespaces/<ns>/<resource> ---

pub(crate) async fn core_create(
    State(st): State<AppState>,
    Path(resource): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    do_create(&st, &Loc::new("", "v1", resource, None), body).await
}

pub(crate) async fn core_create_ns(
    State(st): State<AppState>,
    Path((ns, resource)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> Response {
    do_create(&st, &Loc::new("", "v1", resource, Some(ns)), body).await
}

pub(crate) async fn core_list(
    State(st): State<AppState>,
    Path(resource): Path<String>,
    Query(q): Query<ListParams>,
) -> Response {
    do_list(&st, &Loc::new("", "v1", resource, None), &q).await
}

pub(crate) async fn core_list_ns(
    State(st): State<AppState>,
    Path((ns, resource)): Path<(String, String)>,
    Query(q): Query<ListParams>,
) -> Response {
    do_list(&st, &Loc::new("", "v1", resource, Some(ns)), &q).await
}

// --- grouped: /apis/<g>/<v>/<resource> + /namespaces/<ns>/<resource> ---

pub(crate) async fn grp_create(
    State(st): State<AppState>,
    Path((group, version, resource)): Path<(String, String, String)>,
    Json(body): Json<Value>,
) -> Response {
    do_create(&st, &Loc::new(&group, &version, resource, None), body).await
}

pub(crate) async fn grp_create_ns(
    State(st): State<AppState>,
    Path((group, version, ns, resource)): Path<(String, String, String, String)>,
    Json(body): Json<Value>,
) -> Response {
    do_create(&st, &Loc::new(&group, &version, resource, Some(ns)), body).await
}

pub(crate) async fn grp_list(
    State(st): State<AppState>,
    Path((group, version, resource)): Path<(String, String, String)>,
    Query(q): Query<ListParams>,
) -> Response {
    do_list(&st, &Loc::new(&group, &version, resource, None), &q).await
}

pub(crate) async fn grp_list_ns(
    State(st): State<AppState>,
    Path((group, version, ns, resource)): Path<(String, String, String, String)>,
    Query(q): Query<ListParams>,
) -> Response {
    do_list(&st, &Loc::new(&group, &version, resource, Some(ns)), &q).await
}
