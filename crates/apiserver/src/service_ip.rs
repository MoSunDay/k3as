//! ClusterIP allocation at Service create time (TODO **T4.3 Scope C**,
//! decision **Q44**).
//!
//! Allocation rides the apiserver and mirrors the NodePort CAS self-heal
//! precedent in [`crate::service`]: on Service create a missing
//! `spec.clusterIP` is allocated lowest-free from `--service-cidr` (default
//! [`DEFAULT_SERVICE_CIDR`], the k3s default); an explicit clusterIP is
//! validated (IPv4, inside the CIDR, not already taken); a double
//! allocation lost to a concurrent writer between LIST and CREATE is
//! healed post-create via CAS retry ([`heal_clusterip_collision`]).
//! Headless Services (`clusterIP: "None"`) and `ExternalName` Services are
//! left untouched. The Router carries the VIP traffic on the ClusterIP
//! plane (Q44; see the `clusterip` module there).

use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::fmt;
use std::net::Ipv4Addr;
use std::sync::Arc;
use storage::{Key, KeyPrefix, StorageBackend, StorageError, StoredEntry};

/// Default Service CIDR (k3s parity: `10.43.0.0/16`).
pub const DEFAULT_SERVICE_CIDR: &str = "10.43.0.0/16";

/// Bound on CAS retries when healing a post-create clusterIP collision.
const HEAL_MAX_ATTEMPTS: u32 = 5;

/// A parsed IPv4 Service CIDR: network base (host-order `u32`, host bits
/// masked to 0) + prefix length (mirrors the controllers' node-CIDR parse).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ServiceCidr {
    /// Network address as a host-order `u32` (e.g. `10.43.0.0`).
    base: u32,
    /// Prefix length in bits (`16` for `10.43.0.0/16`).
    prefix: u8,
}

impl Default for ServiceCidr {
    fn default() -> Self {
        // 10.43.0.0/16 — the k3s default service CIDR.
        ServiceCidr {
            base: (10 << 24) | (43 << 16),
            prefix: 16,
        }
    }
}

impl ServiceCidr {
    /// Parse `a.b.c.d/p` into a masked [`ServiceCidr`] (host bits dropped).
    pub fn parse(s: &str) -> Option<ServiceCidr> {
        let (ip, prefix) = s.split_once('/')?;
        let base = parse_ipv4(ip)?;
        if prefix.is_empty() || prefix.bytes().any(|b| !b.is_ascii_digit()) {
            return None;
        }
        let prefix: u8 = prefix.parse().ok()?;
        if prefix > 32 {
            return None;
        }
        Some(ServiceCidr {
            base: base & net_mask(prefix),
            prefix,
        })
    }

    /// True when `ip` falls inside the CIDR.
    pub fn contains(&self, ip: Ipv4Addr) -> bool {
        (u32_of(ip) & net_mask(self.prefix)) == self.base
    }
}

impl fmt::Display for ServiceCidr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", ipv4_string(self.base), self.prefix)
    }
}

/// Net mask for `prefix` (all-ones above the prefix; `/0` -> 0, `/32` -> MAX).
fn net_mask(prefix: u8) -> u32 {
    match prefix {
        0 => 0,
        32 => u32::MAX,
        p => u32::MAX << (32 - u32::from(p)),
    }
}

/// Host mask for `prefix` (the net mask's complement).
fn host_mask(prefix: u8) -> u32 {
    !net_mask(prefix)
}

/// Dotted-quad `a.b.c.d` -> host-order `u32` (digits-only octets 0-255).
fn parse_ipv4(s: &str) -> Option<u32> {
    let mut out: u32 = 0;
    let mut seen = 0;
    for part in s.split('.') {
        if part.is_empty() || part.len() > 3 || part.bytes().any(|b| !b.is_ascii_digit()) {
            return None;
        }
        let octet: u16 = part.parse().ok()?;
        if octet > 255 {
            return None;
        }
        out = (out << 8) | u32::from(octet);
        seen += 1;
    }
    (seen == 4).then_some(out)
}

/// Host-order `u32` -> dotted quad.
fn ipv4_string(ip: u32) -> String {
    format!(
        "{}.{}.{}.{}",
        ip >> 24,
        (ip >> 16) & 0xff,
        (ip >> 8) & 0xff,
        ip & 0xff
    )
}

/// `Ipv4Addr` -> host-order `u32`.
fn u32_of(ip: Ipv4Addr) -> u32 {
    u32::from_be_bytes(ip.octets())
}

/// Host-order `u32` -> `Ipv4Addr`.
fn ip_of(n: u32) -> Ipv4Addr {
    Ipv4Addr::from(n.to_be_bytes())
}

/// The parseable `spec.clusterIP` of one Service (`None` for "None",
/// absent, or garbage).
fn cluster_ip_of(v: &Value) -> Option<Ipv4Addr> {
    v.pointer("/spec/clusterIP")
        .and_then(Value::as_str)
        .and_then(|s| s.parse::<Ipv4Addr>().ok())
}

/// Every parseable `spec.clusterIP` already taken across `services`.
pub(crate) fn used_cluster_ips(services: &[Value]) -> BTreeSet<Ipv4Addr> {
    services.iter().filter_map(cluster_ip_of).collect()
}

/// Lowest free host IP in the CIDR, skipping the all-zeros and all-ones
/// host parts (`/32` and `/31` have no such convention — every address in
/// the block is allocatable, RFC 3021 parity).
fn alloc_free(
    used: &BTreeSet<Ipv4Addr>,
    cidr: &ServiceCidr,
) -> Result<Ipv4Addr, crate::service::ServiceError> {
    let host_bits = 32 - u32::from(cidr.prefix);
    // /32 and /31 carry no network/broadcast convention (RFC 3021): every
    // address in the block is allocatable; wider prefixes skip the
    // all-zeros and all-ones host parts.
    let found = if host_bits <= 1 {
        (cidr.base..=cidr.base + host_bits)
            .map(ip_of)
            .find(|ip| !used.contains(ip))
    } else {
        let broadcast = cidr.base | host_mask(cidr.prefix);
        ((cidr.base + 1)..broadcast)
            .map(ip_of)
            .find(|ip| !used.contains(ip))
    };
    found.ok_or_else(|| {
        crate::service::ServiceError::Invalid(format!("service CIDR {cidr} exhausted"))
    })
}

/// Default/validate `spec.clusterIP` IN PLACE:
///  - `ExternalName` type or absent ports: skip (untouched);
///  - `clusterIP: "None"`: headless, untouched (no allocation);
///  - absent: allocate the lowest free host IP and write it in;
///  - explicit: must parse as IPv4, sit inside `cidr`, and be free.
pub(crate) fn default_cluster_ip(
    body: &mut Value,
    used: &mut BTreeSet<Ipv4Addr>,
    cidr: &ServiceCidr,
) -> Result<(), crate::service::ServiceError> {
    use crate::service::ServiceError;
    let ty = body.pointer("/spec/type").and_then(Value::as_str);
    if ty == Some("ExternalName") {
        return Ok(()); // no VIP: the Service forwards by externalName
    }
    let has_ports = body
        .pointer("/spec/ports")
        .and_then(Value::as_array)
        .is_some_and(|a| !a.is_empty());
    if !has_ports {
        return Ok(()); // portless: nothing to forward, no allocation
    }
    let explicit = body
        .pointer("/spec/clusterIP")
        .filter(|v| !v.is_null())
        .and_then(Value::as_str)
        .map(str::to_string);
    let ip = match explicit.as_deref() {
        Some("None") => return Ok(()), // headless: leave verbatim
        Some(s) => {
            let ip: Ipv4Addr = s.parse().map_err(|_| {
                ServiceError::Invalid(format!(
                    "clusterIP {s:?} is not a valid IPv4 address (service CIDR {cidr})"
                ))
            })?;
            if !cidr.contains(ip) {
                return Err(ServiceError::Invalid(format!(
                    "clusterIP {ip} is outside the service CIDR {cidr}"
                )));
            }
            if used.contains(&ip) {
                return Err(ServiceError::Invalid(format!(
                    "clusterIP {ip} is already allocated"
                )));
            }
            ip
        }
        None => alloc_free(used, cidr)?,
    };
    used.insert(ip);
    if explicit.is_none() {
        if let Some(spec) = body.get_mut("spec").and_then(Value::as_object_mut) {
            spec.insert("clusterIP".into(), json!(ip.to_string()));
        }
    }
    Ok(())
}

/// Create-time Service preparation: LIST services, then default in place.
pub(crate) async fn prepare(
    store: &Arc<dyn StorageBackend>,
    body: &mut Value,
    cidr: &ServiceCidr,
) -> Result<(), crate::service::ServiceError> {
    let mut used = BTreeSet::new();
    if let Ok(existing) = store.list(&KeyPrefix::new("", "services", None)).await {
        let vals: Vec<Value> = existing.into_iter().map(|e| e.value).collect();
        used = used_cluster_ips(&vals);
    }
    default_cluster_ip(body, &mut used, cidr)
}

/// Post-create collision heal: if a DIFFERENT Service holds our clusterIP
/// (lost LIST/CREATE race), CAS-update us onto the lowest free IP (bounded
/// retry). Headless/None, absent, ExternalName, and malformed values pass
/// through; never fails the request — worst case the object keeps its IP.
pub(crate) async fn heal_clusterip_collision(
    store: &Arc<dyn StorageBackend>,
    key: &Key,
    mut entry: StoredEntry,
    cidr: &ServiceCidr,
) -> StoredEntry {
    for _ in 0..HEAL_MAX_ATTEMPTS {
        let Some(ours) = cluster_ip_of(&entry.value) else {
            return entry; // absent / headless "None" / malformed: pass through
        };
        let ty = entry.value.pointer("/spec/type").and_then(Value::as_str);
        if ty == Some("ExternalName") {
            return entry;
        }
        let Ok(listing) = store.list(&KeyPrefix::new("", "services", None)).await else {
            return entry; // give up gracefully
        };
        let foreign: BTreeSet<Ipv4Addr> = listing
            .iter()
            .filter(|e| e.key != key.as_path())
            .filter_map(|e| cluster_ip_of(&e.value))
            .collect();
        if !foreign.contains(&ours) {
            return entry; // no collision
        }
        let Ok(next_ip) = alloc_free(&foreign, cidr) else {
            return entry; // cannot heal (CIDR exhausted): keep as-is
        };
        let mut next = entry.value.clone();
        if let Some(spec) = next.get_mut("spec").and_then(Value::as_object_mut) {
            spec.insert("clusterIP".into(), json!(next_ip.to_string()));
        }
        match store.update(key, next, Some(entry.mod_revision)).await {
            Ok(updated) => return updated,
            Err(StorageError::Conflict { .. }) => {
                entry = match store.get(key).await {
                    Ok(Some(e)) => e, // someone else moved us: re-check
                    _ => return entry,
                };
            }
            Err(_) => return entry, // give up gracefully
        }
    }
    entry
}
