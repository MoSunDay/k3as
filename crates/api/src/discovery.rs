//! API discovery document builders (TODO **T1.1**, step S7 skeleton).
//!
//! Produces the wire documents served at `/api` and `/apis` from a
//! [`SchemaRegistry`]. These are pure functions over the registry — the actual
//! HTTP serving is T1.2; this module gives T1.2 byte-correct bodies today and
//! is unit-tested against the upstream `meta/v1` discovery shapes.
//!
//! - [`core_api_versions`] → served at `/api` (`APIVersions`, core group only).
//! - [`api_group_list`]    → served at `/apis` (`APIGroupList`, non-core groups).
//! - [`api_resource_list`] → served at `/api/<v>` and `/apis/<g>/<v>`
//!   (`APIResourceList` — the per-group/version resource index).

use std::collections::BTreeMap;

use k8s_openapi::apimachinery::pkg::apis::meta::v1::{
    APIGroup, APIGroupList, APIResource, APIResourceList, APIVersions, GroupVersionForDiscovery,
};

use crate::gvk::ApiVersion;
use crate::schema::{Scope, SchemaRegistry, TypeInfo};

/// Build the `/api` response (core `""` group): the list of served versions.
/// `server_address` is the host:port the API server advertises (passed by T1.2).
pub fn core_api_versions(registry: &SchemaRegistry, server_address: &str) -> APIVersions {
    let versions: Vec<String> = registry
        .iter()
        .filter(|(gvk, _)| gvk.group.is_empty())
        .map(|(gvk, _)| gvk.version.clone())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    APIVersions {
        versions,
        server_address_by_client_cidrs: vec![k8s_openapi::apimachinery::pkg::apis::meta::v1::ServerAddressByClientCIDR {
            client_cidr: "0.0.0.0/0".to_string(),
            server_address: server_address.to_string(),
        }],
    }
}

/// Build the `/apis` response: every non-core group served, each with its
/// versions and a preferred version (lexicographically last, deterministic).
pub fn api_group_list(registry: &SchemaRegistry) -> APIGroupList {
    // group -> (BTreeSet<version>)
    let mut groups: BTreeMap<String, std::collections::BTreeSet<String>> = BTreeMap::new();
    for (gvk, _) in registry.iter() {
        if gvk.group.is_empty() {
            continue;
        }
        groups
            .entry(gvk.group.clone())
            .or_default()
            .insert(gvk.version.clone());
    }
    let groups_list: Vec<APIGroup> = groups
        .into_iter()
        .map(|(name, versions)| {
            let preferred = versions.iter().last().cloned().unwrap_or_default();
            APIGroup {
                name: name.clone(),
                versions: versions
                    .iter()
                    .map(|v| GroupVersionForDiscovery {
                        group_version: ApiVersion::new(&name, v).to_string(),
                        version: v.clone(),
                    })
                    .collect(),
                preferred_version: Some(GroupVersionForDiscovery {
                    group_version: ApiVersion::new(&name, &preferred).to_string(),
                    version: preferred,
                }),
                server_address_by_client_cidrs: None,
            }
        })
        .collect();
    APIGroupList { groups: groups_list }
}

/// Build the `/api/<version>` or `/apis/<group>/<version>` resource index.
/// Returns `None` if no resources are registered for that group+version.
pub fn api_resource_list(
    registry: &SchemaRegistry,
    group: &str,
    version: &str,
) -> Option<APIResourceList> {
    let mut resources: Vec<APIResource> = registry
        .iter()
        .filter(|(gvk, _)| gvk.group == group && gvk.version == version)
        .map(|(_, info)| to_api_resource(group, info))
        .collect();
    if resources.is_empty() {
        return None;
    }
    resources.sort_by(|a, b| a.name.cmp(&b.name));
    Some(APIResourceList {
        group_version: ApiVersion::new(group, version).to_string(),
        resources,
    })
}

fn to_api_resource(group: &str, info: &TypeInfo) -> APIResource {
    APIResource {
        name: info.resource.clone(),
        singular_name: lowercase_first(&info.kind),
        namespaced: matches!(info.scope, Scope::Namespaced),
        kind: info.kind.clone(),
        verbs: default_verbs(),
        group: if group.is_empty() { None } else { Some(group.to_string()) },
        version: None,
        storage_version_hash: None,
        categories: None,
        short_names: None,
    }
}

fn default_verbs() -> Vec<String> {
    ["get", "list", "watch", "create", "update", "patch", "delete"]
        .into_iter()
        .map(str::to_string)
        .collect()
}

fn lowercase_first(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(first) => first.to_lowercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::initpro;

    fn registry_with_initpro() -> SchemaRegistry {
        let mut r = SchemaRegistry::with_core_v1();
        initpro::register(&mut r);
        r
    }

    #[test]
    fn core_api_versions_lists_v1() {
        let reg = registry_with_initpro();
        let doc = core_api_versions(&reg, "127.0.0.1:6443");
        assert!(doc.versions.iter().any(|v| v == "v1"));
        assert_eq!(doc.server_address_by_client_cidrs[0].server_address, "127.0.0.1:6443");
    }

    #[test]
    fn api_group_list_includes_initpro_group() {
        let reg = registry_with_initpro();
        let doc = api_group_list(&reg);
        let groups: Vec<&str> = doc.groups.iter().map(|g| g.name.as_str()).collect();
        assert!(groups.contains(&"init-pro.io"), "groups = {groups:?}");
        let initpro_group = doc.groups.iter().find(|g| g.name == "init-pro.io").unwrap();
        assert_eq!(initpro_group.preferred_version.as_ref().unwrap().version, "v1");
        assert_eq!(initpro_group.preferred_version.as_ref().unwrap().group_version, "init-pro.io/v1");
    }

    #[test]
    fn api_resource_list_for_core_v1_lists_pods() {
        let reg = registry_with_initpro();
        let doc = api_resource_list(&reg, "", "v1").expect("core v1 served");
        assert_eq!(doc.group_version, "v1");
        let pod = doc.resources.iter().find(|r| r.name == "pods").unwrap();
        assert_eq!(pod.kind, "Pod");
        assert!(pod.namespaced);
        assert!(pod.verbs.contains(&"watch".to_string()));
        assert!(pod.namespaced); // pods are namespaced
    }

    #[test]
    fn api_resource_list_for_initpro_v1_lists_luarouters() {
        let reg = registry_with_initpro();
        let doc = api_resource_list(&reg, "init-pro.io", "v1").expect("init-pro.io/v1 served");
        let lr = doc.resources.iter().find(|r| r.name == "luarouters").unwrap();
        assert_eq!(lr.kind, "LuaRouter");
        assert_eq!(lr.singular_name, "luaRouter");
        assert!(lr.namespaced);
    }

    #[test]
    fn api_resource_list_returns_none_for_unknown_group() {
        let reg = registry_with_initpro();
        assert!(api_resource_list(&reg, "nope.example", "v1").is_none());
    }

    #[test]
    fn discovery_documents_round_trip_through_json() {
        // The discovery docs themselves must be JSON-lossless (Q10).
        let reg = registry_with_initpro();
        let gl = api_group_list(&reg);
        let s = serde_json::to_string(&gl).unwrap();
        let back: APIGroupList = serde_json::from_str(&s).unwrap();
        assert_eq!(gl.groups.len(), back.groups.len());
        let av = core_api_versions(&reg, "127.0.0.1:6443");
        let s2 = serde_json::to_string(&av).unwrap();
        let back2: APIVersions = serde_json::from_str(&s2).unwrap();
        assert_eq!(av.versions, back2.versions);
    }
}
