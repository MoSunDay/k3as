//! `kubectl get|apply|delete` for `argoproj.io/v1alpha1` (T7.3 Scope A,
//! **Q30**) and the first-class `init-pro.io/v1` `Agent` (T7.1, **Q35**).
//!
//! Three layers, mirroring `rollout.rs`:
//! - pure helpers ([`resolve_target`], [`resolve_plural`],
//!   [`resource_path`], [`parse_output`],
//!   [`age_between`], [`render_table`], [`parse_manifest`],
//!   [`manifest_target`]) — unit-tested with no server in sight;
//! - one verb function per subcommand that drives the Q21 HTTP client;
//! - SSA note: `apply` PATCHes with `Content-Type: application/apply-patch+yaml`
//!   and a JSON body (the server parses JSON bodies regardless of that
//!   content type — Q10 wire rule, see crates/apiserver/tests/rest_apply.rs).

use serde::Deserialize;
use serde_json::Value;

use crate::http::HttpClient;

/// Group/version for the built-in `Agent` resource (T7.1, **Q35**).
pub(crate) const AGENT_API_VERSION: &str = "init-pro.io/v1";
/// Group/version for the argo Workflow family (T7.3 Scope A, Q30).
pub(crate) const ARGO_API_VERSION: &str = "argoproj.io/v1alpha1";
/// SSA field manager identity (mirrors kubectl's `kubectl-client-side-apply`
/// convention; visible in `metadata.managedFields`).
const FIELD_MANAGER: &str = "init-pro-kubectl";
/// SSA media type — the body we send is JSON (Q10; the server accepts it).
const APPLY_CONTENT_TYPE: &str = "application/apply-patch+yaml";
const JSON_CONTENT_TYPE: &str = "application/json";

/// Map a user-supplied kind/alias to its `(apiVersion, plural)` REST
/// target — `Agent` lives in **our own** `init-pro.io/v1` group (T7.1,
/// Q35), the Workflow family stays in `argoproj.io/v1alpha1` (T7.3, Q30).
pub(crate) fn resolve_target(kind: &str) -> Result<(&'static str, &'static str), String> {
    match kind.to_ascii_lowercase().as_str() {
        "agent" | "agents" | "ag" => Ok((AGENT_API_VERSION, "agents")),
        "workflow" | "workflows" | "wf" => Ok((ARGO_API_VERSION, "workflows")),
        "cronworkflow" | "cronworkflows" | "cwf" | "cronwf" => {
            Ok((ARGO_API_VERSION, "cronworkflows"))
        }
        other => Err(format!(
            "unsupported resource kind {other:?}: argoproj.io/v1alpha1 supports \
             workflow (aliases: workflows, wf) and cronworkflow \
             (aliases: cronworkflows, cwf, cronwf) in v1 (T7.3 Scope A); \
             init-pro.io/v1 supports agent (aliases: agents, ag) in v1 \
             (T7.1, Q35)"
        )),
    }
}

/// Map a user-supplied kind/alias to the plural (delegates to
/// [`resolve_target`]); retained for callers that only need the plural
/// segment (currently the unit tests; keep in the non-test build so the
/// whitelist seam stays exercisable).
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn resolve_plural(kind: &str) -> Result<&'static str, String> {
    resolve_target(kind).map(|(_, plural)| plural)
}

/// Namespaced collection/item REST path for an `(apiVersion, plural)`
/// target: `/apis/{api_version}/namespaces/{ns}/{plural}[/{name}]`.
pub(crate) fn resource_path(
    api_version: &str,
    plural: &str,
    ns: &str,
    name: Option<&str>,
) -> String {
    match name {
        Some(name) => format!("/apis/{api_version}/namespaces/{ns}/{plural}/{name}"),
        None => format!("/apis/{api_version}/namespaces/{ns}/{plural}"),
    }
}

/// `-o` output format for `get`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Output {
    /// kubectl-style `NAME  PHASE  AGE` table (default).
    Table,
    /// Pretty-printed JSON of the object/list.
    Json,
    /// YAML of the object/list.
    Yaml,
    /// `<kind>[.<group>]/<name>` per line — kubectl `-o name` parity
    /// (`workflow.argoproj.io/hello` for grouped, `pod/nginx` for core).
    Name,
}

/// Parse an `-o` value; empty/table/wide fall back to the table default.
pub(crate) fn parse_output(raw: &str) -> Result<Output, String> {
    match raw {
        "" | "table" | "wide" => Ok(Output::Table),
        "json" => Ok(Output::Json),
        "yaml" | "yml" => Ok(Output::Yaml),
        "name" => Ok(Output::Name),
        other => Err(format!(
            "invalid output format {other:?}: v1 supports json, yaml, name (T7.3 Scope A)"
        )),
    }
}

/// Days since 1970-01-01, proleptic Gregorian (Hinnant's civil algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let mp = if m > 2 { m - 3 } else { m + 9 }; // Mar=0..Feb=11
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Parse `YYYY-MM-DDTHH:MM:SS[.frac][Z]` to epoch seconds. Kubernetes
/// `creationTimestamp` is always UTC `Z`-suffixed, so any suffix (`Z`,
/// fractional digits, or a numeric offset) is ignored rather than honored —
/// documented v1 simplification.
fn rfc3339_to_epoch(ts: &str) -> Option<i64> {
    let b = ts.as_bytes();
    let bad = b.len() < 19
        || b[4] != b'-'
        || b[7] != b'-'
        || (b[10] != b'T' && b[10] != b' ')
        || b[13] != b':'
        || b[16] != b':';
    if bad {
        return None;
    }
    let nums = [
        ts.get(0..4)?.parse::<i64>().ok()?,
        ts.get(5..7)?.parse::<i64>().ok()?,
        ts.get(8..10)?.parse::<i64>().ok()?,
        ts.get(11..13)?.parse::<i64>().ok()?,
        ts.get(14..16)?.parse::<i64>().ok()?,
        ts.get(17..19)?.parse::<i64>().ok()?,
    ];
    let [y, mo, d, h, mi, s] = nums;
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || s > 60 {
        return None;
    }
    Some(days_from_civil(y, mo, d) * 86_400 + h * 3_600 + mi * 60 + s)
}

/// Wall-clock epoch seconds (the only impure age ingredient).
pub(crate) fn now_epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// `AGE` column value: coarsest single unit (`5s`, `10m`, `3h`, `2d`),
/// `<unknown>` when the timestamp is absent or unparseable. Pure over
/// `now_secs` so tests pin both ends.
pub(crate) fn age_between(ts: Option<&str>, now_secs: i64) -> String {
    let Some(created) = ts.and_then(rfc3339_to_epoch) else {
        return "<unknown>".to_string();
    };
    let secs = (now_secs - created).max(0);
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3_600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3_600)
    } else {
        format!("{}d", secs / 86_400)
    }
}

/// Render the `NAME   PHASE   AGE` table: column width = widest cell
/// including the header, 3-space gap, last column unpadded. Phase falls
/// back to `Unknown` (CronWorkflows often carry no `status.phase`), age to
/// `<unknown>`.
pub(crate) fn render_table(items: &[Value], now_secs: i64) -> String {
    let mut rows: Vec<[String; 3]> = vec![["NAME".into(), "PHASE".into(), "AGE".into()]];
    for v in items {
        rows.push([
            v["metadata"]["name"]
                .as_str()
                .unwrap_or("<unknown>")
                .to_string(),
            v["status"]["phase"]
                .as_str()
                .unwrap_or("Unknown")
                .to_string(),
            age_between(v["metadata"]["creationTimestamp"].as_str(), now_secs),
        ]);
    }
    let widths: [usize; 3] =
        std::array::from_fn(|c| rows.iter().map(|r| r[c].len()).max().unwrap_or(0));
    rows.iter()
        .map(|r| {
            format!(
                "{:<w0$}   {:<w1$}   {}",
                r[0],
                r[1],
                r[2],
                w0 = widths[0],
                w1 = widths[1]
            )
            .trim_end()
            .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

/// Parse exactly ONE YAML (or JSON — YAML superset) manifest document.
pub(crate) fn parse_manifest(raw: &str, source: &str) -> Result<Value, String> {
    if raw.trim().is_empty() {
        return Err(format!("no YAML document found in {source}"));
    }
    let mut docs = serde_yaml_ng::Deserializer::from_str(raw);
    let Some(first) = docs.next() else {
        return Err(format!("no YAML document found in {source}"));
    };
    let obj = Value::deserialize(first).map_err(|e| format!("invalid YAML in {source}: {e}"))?;
    if docs.next().is_some() {
        return Err(format!(
            "multi-document YAML not supported in v1: {source} has more than one document \
             (T7.3 Scope A)"
        ));
    }
    if !obj.is_object() {
        return Err(format!("manifest in {source} must be a YAML mapping"));
    }
    Ok(obj)
}

/// `(kind, name, generate_name, namespace_override)` from a manifest.
pub(crate) type ManifestTarget = (String, Option<String>, Option<String>, Option<String>);

/// Extract the target from a manifest (pure). `name` is `None` for
/// generateName-only manifests (created via POST; the server picks suffix).
pub(crate) fn manifest_target(obj: &Value) -> Result<ManifestTarget, String> {
    let kind = obj["kind"]
        .as_str()
        .filter(|k| !k.is_empty())
        .ok_or("manifest must set .kind (Workflow or CronWorkflow)")?;
    let name = obj["metadata"]["name"]
        .as_str()
        .filter(|n| !n.is_empty())
        .map(str::to_string);
    let generate_name = obj["metadata"]["generateName"]
        .as_str()
        .filter(|n| !n.is_empty())
        .map(str::to_string);
    if name.is_none() && generate_name.is_none() {
        return Err("manifest must set .metadata.name (or .metadata.generateName)".into());
    }
    let ns = obj["metadata"]["namespace"]
        .as_str()
        .filter(|n| !n.is_empty());
    Ok((
        kind.to_string(),
        name,
        generate_name,
        ns.map(str::to_string),
    ))
}

/// kubectl `-o name` line: `kind.group/name` for grouped resources,
/// `kind/name` for core (empty group).
fn qualified_name(item: &Value) -> String {
    let name = item["metadata"]["name"].as_str().unwrap_or("?");
    let kind = item["kind"].as_str().unwrap_or("").to_lowercase();
    let group = item["apiVersion"]
        .as_str()
        .and_then(|v| v.split_once('/'))
        .map(|(g, _)| g)
        .unwrap_or("");
    if group.is_empty() {
        format!("{kind}/{name}")
    } else {
        format!("{kind}.{group}/{name}")
    }
}

/// `kubectl get`: single object (name given) or collection listing.
pub(crate) async fn get(
    http: &HttpClient,
    ns: &str,
    api_version: &str,
    plural: &str,
    name: Option<&str>,
    output: Output,
) -> Result<String, String> {
    let (code, body) = http
        .get_json(&resource_path(api_version, plural, ns, name))
        .await?;
    if code == 404 {
        let what = name
            .map(|n| format!("{plural}/{n}"))
            .unwrap_or_else(|| format!("the requested {plural} in namespace {ns:?}"));
        return Err(format!("{what} not found"));
    }
    if !(200..300).contains(&code) {
        return Err(format!("get failed: HTTP {code} {body}"));
    }
    let items: Vec<Value> = if name.is_some() {
        vec![body.clone()]
    } else {
        body["items"].as_array().cloned().unwrap_or_default()
    };
    match output {
        Output::Table if items.is_empty() => Ok(format!("No resources found in {ns} namespace.\n")),
        Output::Table => Ok(render_table(&items, now_epoch())),
        Output::Json => serde_json::to_string_pretty(&body)
            .map(|s| s + "\n")
            .map_err(|e| format!("render json: {e}")),
        Output::Yaml => serde_yaml_ng::to_string(&body).map_err(|e| format!("render yaml: {e}")),
        Output::Name => Ok(items
            .iter()
            .map(qualified_name)
            .map(|n| format!("{n}\n"))
            .collect()),
    }
}

/// `kubectl apply -f FILE`: SSA-PATCH the single manifest (201 create /
/// 200 update / 409 conflict, see rest_apply.rs).
pub(crate) async fn apply(http: &HttpClient, ns: &str, file: &str) -> Result<String, String> {
    let raw =
        std::fs::read_to_string(file).map_err(|e| format!("cannot read file {file:?}: {e}"))?;
    let obj = parse_manifest(&raw, file)?;
    let (kind, name, _generate_name, ns_override) = manifest_target(&obj)?;
    // Agents resolve to init-pro.io/v1, Workflow family to
    // argoproj.io/v1alpha1 (T7.1/Q35 + T7.3/Q30) — the manifest body itself
    // passes through generically (YAML/JSON -> JSON, Q10).
    let (api_version, plural) = resolve_target(&kind)?;
    // Namespace precedence: manifest `.metadata.namespace` > `-n` flag.
    let target_ns = ns_override.unwrap_or_else(|| ns.to_string());
    let body = serde_json::to_string(&obj).map_err(|e| format!("serialize manifest: {e}"))?;
    // generateName-only manifests cannot be SSA-PATCHed (no URL name); real
    // kubectl falls back to a plain create and lets the server pick the
    // suffix (T7.3 Scope A).
    let Some(name) = name else {
        let path = resource_path(api_version, plural, &target_ns, None);
        let (code, resp) = http
            .request_json("POST", &path, Some(JSON_CONTENT_TYPE), Some(&body))
            .await?;
        return match code {
            201 => Ok(format!(
                "{kind}/{} created",
                resp["metadata"]["name"].as_str().unwrap_or("?")
            )),
            code => Err(format!("apply failed: HTTP {code} {resp}")),
        };
    };
    let path = format!(
        "{}?fieldManager={FIELD_MANAGER}",
        resource_path(api_version, plural, &target_ns, Some(&name))
    );
    let (code, resp) = http
        .request_json("PATCH", &path, Some(APPLY_CONTENT_TYPE), Some(&body))
        .await?;
    match code {
        200 | 201 => Ok(format!("{kind}/{name} applied")),
        409 => Err(format!(
            "apply conflict on {kind}/{name}: {}",
            resp["message"].as_str().unwrap_or("field manager conflict")
        )),
        code => Err(format!("apply failed: HTTP {code} {resp}")),
    }
}

/// `kubectl delete TYPE/NAME` (or `TYPE NAME`).
pub(crate) async fn delete(
    http: &HttpClient,
    ns: &str,
    api_version: &str,
    plural: &str,
    name: &str,
) -> Result<String, String> {
    let (code, body) = http
        .request_json(
            "DELETE",
            &resource_path(api_version, plural, ns, Some(name)),
            None,
            None,
        )
        .await?;
    match code {
        200..=299 => Ok(format!("{plural}/{name} deleted")),
        404 => Err(format!("{plural}/{name} not found")),
        code => Err(format!("delete failed: HTTP {code} {body}")),
    }
}
