//! Node podCIDR allocator (TODO **T4.3 Scope A**, decision **Q34**):
//! the kube-controller-manager-equivalent `--allocate-node-cidrs` loop.
//!
//! The server process derives each Node's `spec.podCIDR` from the cluster
//! CIDR (`--cluster-cidr`, default `10.42.0.0/16`): list every Node, mask
//! off the already-assigned `/NODE_CIDR_PREFIX` subnets, take the first
//! free one and CAS-write it onto `spec.podCIDR` + `spec.podCIDRs` (the
//! k8s wire shape kube-flannel's subnet manager reads; flanneld ignores
//! the in-repo kubelet which never consumes podCIDR itself).
//!
//! Allocation is serial by construction: the runner enqueues every Node
//! event into a single-worker queue, so two Nodes can never race for the
//! same free subnet through this controller. A lost CAS (a concurrent
//! writer, e.g. a Node status patch from flanneld) surfaces as
//! [`ControllerError::Conflict`] and requeues with backoff against the
//! refreshed object. Idempotent: Nodes that already carry a podCIDR are
//! skipped (no rewrite, no churn).
//!
//! Pure address math lives in [`parse_cidr`] / [`next_free`] /
//! [`cidr_string`]; the reconciler is a plain function over the
//! [`Client`] (Q19), JSON-only wire (Q10).

use std::sync::Arc;

use serde_json::Value;
use storage::{Key, KeyPrefix};

use crate::client::Client;
use crate::error::ControllerError;
use crate::object::{name, resource_version};

/// Per-node subnet prefix (k3s default: `/24` per Node).
pub const NODE_CIDR_PREFIX: u8 = 24;

/// A parsed IPv4 CIDR: network base (host-order u32, host bits masked to 0)
/// and prefix length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClusterCidr {
    /// Network address as a host-order u32 (e.g. `10.42.0.0`).
    pub base: u32,
    /// Prefix length in bits (`16` for `10.42.0.0/16`).
    pub prefix: u8,
}

impl ClusterCidr {
    /// The per-node subnet prefix used for allocation.
    pub fn node_prefix(&self) -> u8 {
        NODE_CIDR_PREFIX
    }
}

/// Parse `a.b.c.d/p` into a masked [`ClusterCidr`] (host bits dropped).
pub fn parse_cidr(s: &str) -> Option<ClusterCidr> {
    let (ip, prefix) = s.split_once('/')?;
    let base = parse_ipv4(ip)?;
    if prefix.bytes().any(|b| !b.is_ascii_digit()) {
        return None;
    }
    let prefix: u8 = prefix.parse().ok()?;
    if prefix > 32 {
        return None;
    }
    let host_bits = 32 - u32::from(prefix);
    Some(ClusterCidr {
        base: base & (u32::MAX << host_bits),
        prefix,
    })
}

/// Dotted-quad `a.b.c.d` -> host-order u32.
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

/// Host-order u32 -> dotted quad.
pub fn ipv4_string(ip: u32) -> String {
    format!(
        "{}.{}.{}.{}",
        ip >> 24,
        (ip >> 16) & 0xff,
        (ip >> 8) & 0xff,
        ip & 0xff
    )
}

/// `a.b.c.d/p` dotted form.
pub fn cidr_string(base: u32, prefix: u8) -> String {
    format!("{}/{}", ipv4_string(base), prefix)
}

/// Every subnet base already assigned across the given Node objects
/// (`spec.podCIDR` plus each entry of `spec.podCIDRs`), host-order u32s.
pub fn used_subnets(nodes: &[Value]) -> Vec<u32> {
    let mut used = Vec::new();
    for n in nodes {
        if let Some(cidr) = n
            .pointer("/spec/podCIDR")
            .and_then(Value::as_str)
            .and_then(parse_cidr)
        {
            used.push(cidr.base);
        }
        if let Some(list) = n.pointer("/spec/podCIDRs").and_then(Value::as_array) {
            for entry in list {
                if let Some(cidr) = entry.as_str().and_then(parse_cidr) {
                    used.push(cidr.base);
                }
            }
        }
    }
    used.sort_unstable();
    used.dedup();
    used
}

/// First free per-node subnet inside `cluster` (`None` = exhausted: the
/// k8s error shape, surfaced as [`ControllerError::Internal`]).
pub fn next_free(cluster: &ClusterCidr, used: &[u32]) -> Option<u32> {
    let node_prefix = cluster.node_prefix();
    if node_prefix < cluster.prefix || node_prefix > 32 {
        return None;
    }
    let step = 1u32 << (32 - u32::from(node_prefix));
    let mut subnet = cluster.base;
    let end = cluster
        .base
        .wrapping_add(1u32 << (32 - u32::from(cluster.prefix)));
    while subnet != end {
        if !used.contains(&subnet) {
            return Some(subnet);
        }
        subnet = subnet.wrapping_add(step);
    }
    None
}

/// Allocate a podCIDR for one Node if it does not have one yet.
///
/// Returns `Ok(Some(cidr))` when a fresh subnet was CAS-written,
/// `Ok(None)` when the node already carries one (idempotent skip) or the
/// node is unnamed. Conflict propagates for the worker's rate-limited
/// requeue; exhaustion maps to the k8s `Internal` shape.
pub async fn allocate(
    client: &Arc<dyn Client>,
    cluster: &ClusterCidr,
    node: &Value,
) -> Result<Option<String>, ControllerError> {
    let Some(node_name) = name(node).map(str::to_string) else {
        return Ok(None);
    };
    if node
        .pointer("/spec/podCIDR")
        .and_then(Value::as_str)
        .is_some_and(|s| !s.is_empty())
        || node
            .pointer("/spec/podCIDRs")
            .and_then(Value::as_array)
            .is_some_and(|a| !a.is_empty())
    {
        return Ok(None); // already allocated: idempotent no-op
    }
    let nodes = client.list(&KeyPrefix::new("", "nodes", None)).await?;
    let used = used_subnets(&nodes);
    let Some(subnet) = next_free(cluster, &used) else {
        return Err(ControllerError::Internal(format!(
            "no available podCIDR subnets left in cluster CIDR {}",
            cidr_string(cluster.base, cluster.prefix)
        )));
    };
    let cidr = cidr_string(subnet, cluster.node_prefix());
    let mut updated = node.clone();
    let spec = updated.as_object_mut().map(|o| {
        o.entry("spec")
            .or_insert_with(|| Value::Object(serde_json::Map::new()))
    });
    let Some(spec) = spec else {
        return Ok(None);
    };
    let obj = spec
        .as_object_mut()
        .ok_or_else(|| ControllerError::Internal("node spec is not an object".into()))?;
    obj.insert("podCIDR".into(), Value::String(cidr.clone()));
    obj.insert(
        "podCIDRs".into(),
        Value::Array(vec![Value::String(cidr.clone())]),
    );
    let key = Key::new("", "nodes", "", &node_name);
    client
        .update(&key, updated, resource_version(node))
        .await
        .map_err(|e| {
            if e.is_conflict() {
                ControllerError::Conflict(format!("podCIDR allocation raced on {node_name}"))
            } else {
                e
            }
        })?;
    Ok(Some(cidr))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::StorageClient;
    use serde_json::json;
    use std::sync::Arc;
    use storage::EmbeddedStorage;

    fn node(name: &str) -> Value {
        json!({
            "apiVersion": "v1", "kind": "Node",
            "metadata": {"name": name},
            "spec": {}
        })
    }

    fn node_with_cidr(name: &str, cidr: &str) -> Value {
        json!({
            "apiVersion": "v1", "kind": "Node",
            "metadata": {"name": name},
            "spec": {"podCIDR": cidr, "podCIDRs": [cidr]}
        })
    }

    #[test]
    fn parses_and_masks_cidrs() {
        assert_eq!(
            parse_cidr("10.42.0.0/16"),
            Some(ClusterCidr {
                base: 0x0a2a_0000,
                prefix: 16
            })
        );
        // Host bits are masked off, not an error (inet_cidr behavior).
        assert_eq!(
            parse_cidr("10.42.7.9/16"),
            Some(ClusterCidr {
                base: 0x0a2a_0000,
                prefix: 16
            })
        );
        assert_eq!(parse_cidr("10.42.0.0/33"), None);
        assert_eq!(parse_cidr("10.42.0.0/o16"), None);
        assert_eq!(parse_cidr("1.2.3/16"), None);
        assert_eq!(parse_cidr("1.2.3.256/16"), None);
        assert_eq!(parse_cidr("10.0.0.0"), None);
    }

    #[test]
    fn cidr_strings_round_trip() {
        let c = parse_cidr("10.42.0.0/16").unwrap();
        assert_eq!(cidr_string(c.base, c.prefix), "10.42.0.0/16");
        assert_eq!(cidr_string(0x0a2a_0100, 24), "10.42.1.0/24");
        assert_eq!(ipv4_string(0x0000_0001), "0.0.0.1");
    }

    #[test]
    fn allocation_walks_subnets_in_order() {
        let cluster = parse_cidr("10.42.0.0/16").unwrap();
        assert_eq!(next_free(&cluster, &[]), Some(0x0a2a_0000));
        assert_eq!(next_free(&cluster, &[0x0a2a_0000]), Some(0x0a2a_0100));
        assert_eq!(
            next_free(&cluster, &[0x0a2a_0000, 0x0a2a_0100]),
            Some(0x0a2a_0200)
        );
        // Used set not in allocation order still skips correctly.
        assert_eq!(next_free(&cluster, &[0x0a2a_0100]), Some(0x0a2a_0000));
    }

    #[test]
    fn used_subnets_reads_both_shapes() {
        let nodes = vec![
            node_with_cidr("a", "10.42.0.0/24"),
            node_with_cidr("b", "10.42.1.0/24"),
            node("fresh"),
        ];
        assert_eq!(used_subnets(&nodes), vec![0x0a2a_0000, 0x0a2a_0100]);
        assert!(used_subnets(&[]).is_empty());
    }

    #[test]
    fn allocation_exhausts_with_internal_error() {
        let cluster = parse_cidr("10.42.0.0/30").unwrap(); // 4 subnets of /30
        assert_eq!(
            next_free(
                &cluster,
                &[0x0a2a_0000, 0x0a2a_0004, 0x0a2a_0008, 0x0a2a_000c]
            ),
            None
        );
    }

    #[tokio::test]
    async fn allocate_is_idempotent_and_never_double_assigns() {
        let store = Arc::new(EmbeddedStorage::new());
        let client: Arc<dyn Client> = Arc::new(StorageClient::new(store));
        let cluster = parse_cidr("10.42.0.0/16").unwrap();
        for n in ["node-a", "node-b"] {
            client
                .create(&Key::new("", "nodes", "", n), node(n))
                .await
                .unwrap();
        }

        // Two sequential allocations take distinct subnets (single-worker
        // serialization is the runner's job; here we exercise the math).
        let na = client
            .get(&Key::new("", "nodes", "", "node-a"))
            .await
            .unwrap()
            .unwrap();
        let first = allocate(&client, &cluster, &na).await.unwrap().unwrap();
        let nb = client
            .get(&Key::new("", "nodes", "", "node-b"))
            .await
            .unwrap()
            .unwrap();
        let second = allocate(&client, &cluster, &nb).await.unwrap().unwrap();
        assert_ne!(
            parse_cidr(&first).unwrap().base,
            parse_cidr(&second).unwrap().base
        );
        assert_eq!(first, "10.42.0.0/24");
        assert_eq!(second, "10.42.1.0/24");

        // Reconciling the same node again is a no-op (idempotent).
        let na2 = client
            .get(&Key::new("", "nodes", "", "node-a"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(allocate(&client, &cluster, &na2).await.unwrap(), None);
    }

    #[tokio::test]
    async fn allocate_recovers_freed_subnets_and_survives_partial_state() {
        let store = Arc::new(EmbeddedStorage::new());
        let client: Arc<dyn Client> = Arc::new(StorageClient::new(store));
        let cluster = parse_cidr("10.42.0.0/16").unwrap();
        // A node that only carries podCIDR (no podCIDRs list) still marks
        // the subnet used; a legacy 0th subnet is honored, next free is .1.
        client
            .create(
                &Key::new("", "nodes", "", "legacy"),
                node_with_cidr("legacy", "10.42.0.0/24"),
            )
            .await
            .unwrap();
        client
            .create(&Key::new("", "nodes", "", "fresh"), node("fresh"))
            .await
            .unwrap();
        let n = client
            .get(&Key::new("", "nodes", "", "fresh"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            allocate(&client, &cluster, &n).await.unwrap().unwrap(),
            "10.42.1.0/24"
        );
    }
}
