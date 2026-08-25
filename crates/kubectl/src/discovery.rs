//! `kubectl api-versions` + `kubectl api-resources` — the discovery-driven
//! listing verbs (T7.3 optional enhancement, **Q30**; acceptance: a fresh
//! cluster lists `workflows`/`cronworkflows` from `argoproj.io/v1alpha1`).
//!
//! Same layering as `verbs.rs`:
//! - pure helpers ([`collect_group_versions`], [`resources_path`],
//!   [`resource_rows`], [`sorted_rows`], [`render_api_versions`],
//!   [`render_api_resources`]) — unit-tested without a server;
//! - two thin async command fns driving the Q21 [`HttpClient`]:
//!   `api_versions` (GET `/api` + GET `/apis`) and `api_resources`
//!   (GET `/apis`, GET `/api`, then one GET per listed group/version,
//!   `preferredVersion` when a group serves several).
//!
//! Upstream filters (`--namespaced`, `--verbs`, `--api-group`) and
//! `--cached` are out of scope for v1; the shared flag parser in lib.rs
//! rejects them like every other unknown flag.

use serde_json::Value;

use crate::http::HttpClient;

/// One `api-resources` table row (pure data; upstream column order:
/// NAME SHORTNAMES APIVERSION NAMESPACED KIND).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResourceRow {
    pub(crate) name: String,
    /// Comma-joined `shortNames` (empty string when the resource has none).
    pub(crate) short_names: String,
    pub(crate) api_version: String,
    pub(crate) namespaced: bool,
    pub(crate) kind: String,
}

/// All served `group/version` strings for `api-versions`: the core
/// versions from `GET /api` (`v1`) plus one entry per `/apis` group
/// (`preferredVersion.groupVersion`, falling back to the first listed
/// version). Sorted alphabetically and de-duplicated (pure).
pub(crate) fn collect_group_versions(core: &Value, groups: &Value) -> Vec<String> {
    let mut versions: Vec<String> = core["versions"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    for group in groups["groups"].as_array().into_iter().flatten() {
        let gv = group["preferredVersion"]["groupVersion"]
            .as_str()
            .or_else(|| group["versions"][0]["groupVersion"].as_str());
        if let Some(gv) = gv {
            versions.push(gv.to_string());
        }
    }
    versions.sort();
    versions.dedup();
    versions
}

/// REST path of the `APIResourceList` for a `group/version`: `/api/v1`
/// for the core group (no `/`), `/apis/<g>/<v>` otherwise.
pub(crate) fn resources_path(group_version: &str) -> String {
    match group_version.split_once('/') {
        None => format!("/api/{group_version}"),
        Some(_) => format!("/apis/{group_version}"),
    }
}

/// Parse an `APIResourceList` into rows, skipping subresources (`name`
/// contains `/`, e.g. `pods/status` — upstream api-resources hides them).
/// Empty/missing fields fall back to kubectl-ish blanks rather than
/// erroring on a partially-filled discovery document (pure).
pub(crate) fn resource_rows(list: &Value, group_version: &str) -> Vec<ResourceRow> {
    list["resources"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|r| {
            r["name"]
                .as_str()
                .map(|n| !n.contains('/'))
                .unwrap_or(false)
        })
        .map(|r| ResourceRow {
            name: r["name"].as_str().unwrap_or_default().to_string(),
            short_names: r["shortNames"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|s| s.as_str())
                        .collect::<Vec<_>>()
                        .join(",")
                })
                .unwrap_or_default(),
            api_version: group_version.to_string(),
            namespaced: r["namespaced"].as_bool().unwrap_or(false),
            kind: r["kind"].as_str().unwrap_or_default().to_string(),
        })
        .collect()
}

/// Rows sorted by NAME (upstream sorts by name within the combined
/// listing); returns a new Vec, input untouched (pure).
pub(crate) fn sorted_rows(rows: &[ResourceRow]) -> Vec<ResourceRow> {
    let mut sorted = rows.to_vec();
    sorted.sort_by(|a, b| a.name.cmp(&b.name));
    sorted
}

/// `api-versions` output: one `group/version` per line, sorted (pure).
pub(crate) fn render_api_versions(versions: &[String]) -> String {
    versions
        .iter()
        .map(|v| format!("{v}\n"))
        .collect::<Vec<_>>()
        .join("")
}

/// `api-resources` output: the `NAME SHORTNAMES APIVERSION NAMESPACED
/// KIND` table, same alignment style as `verbs::render_table` (dynamic
/// column widths, 3-space gap, last column unpadded, lines trimmed).
pub(crate) fn render_api_resources(rows: &[ResourceRow]) -> String {
    let table: Vec<[String; 5]> = std::iter::once([
        "NAME".into(),
        "SHORTNAMES".into(),
        "APIVERSION".into(),
        "NAMESPACED".into(),
        "KIND".into(),
    ])
    .chain(rows.iter().map(|r| {
        [
            r.name.clone(),
            r.short_names.clone(),
            r.api_version.clone(),
            r.namespaced.to_string(),
            r.kind.clone(),
        ]
    }))
    .collect();
    let widths: [usize; 4] =
        std::array::from_fn(|c| table.iter().map(|r| r[c].len()).max().unwrap_or(0));
    table
        .iter()
        .map(|r| {
            format!(
                "{:<w0$}   {:<w1$}   {:<w2$}   {:<w3$}   {}",
                r[0],
                r[1],
                r[2],
                r[3],
                r[4],
                w0 = widths[0],
                w1 = widths[1],
                w2 = widths[2],
                w3 = widths[3],
            )
            .trim_end()
            .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

/// `kubectl api-versions`: GET `/api` + GET `/apis`, print sorted
/// `group/version` lines (upstream includes `v1` for the core group).
pub(crate) async fn api_versions(http: &HttpClient) -> Result<String, String> {
    let (core, groups) = fetch_discovery_roots(http).await?;
    Ok(render_api_versions(&collect_group_versions(&core, &groups)))
}

/// `kubectl api-resources`: list every served group/version (core + the
/// `/apis` groups at their `preferredVersion`), fetch each
/// `APIResourceList`, and print the combined NAME-sorted table. A 404 on
/// one group/version is skipped (the group may have been served by
/// another member); other failures abort like the other verbs.
pub(crate) async fn api_resources(http: &HttpClient) -> Result<String, String> {
    let (core, groups) = fetch_discovery_roots(http).await?;
    let mut rows: Vec<ResourceRow> = Vec::new();
    for gv in collect_group_versions(&core, &groups) {
        let path = resources_path(&gv);
        let (code, list) = http.get_json(&path).await?;
        match code {
            200..=299 => rows.extend(resource_rows(&list, &gv)),
            404 => continue,
            code => return Err(format!("GET {path} failed: HTTP {code} {list}")),
        }
    }
    Ok(render_api_resources(&sorted_rows(&rows)))
}

/// Fetch the two discovery roots (`GET /api`, `GET /apis`) with uniform
/// error rendering; both must succeed for either listing verb.
async fn fetch_discovery_roots(http: &HttpClient) -> Result<(Value, Value), String> {
    let (core_code, core) = http.get_json("/api").await?;
    if !(200..300).contains(&core_code) {
        return Err(format!("GET /api failed: HTTP {core_code} {core}"));
    }
    let (groups_code, groups) = http.get_json("/apis").await?;
    if !(200..300).contains(&groups_code) {
        return Err(format!("GET /apis failed: HTTP {groups_code} {groups}"));
    }
    Ok((core, groups))
}
