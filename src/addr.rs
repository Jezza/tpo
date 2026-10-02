//! Address, index and MAC allocation (spec §3.4).

use crate::model::{ModelError, Topology};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fmt;
use std::net::{Ipv4Addr, Ipv6Addr};

/// A node's (or the gateway's) addresses. `index` is the host part of `v4`; it also names the
/// node's tc classes and filters, so it must stay stable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Allocation {
    pub index: u32,
    pub v4: Ipv4Addr,
    pub v6: Ipv6Addr,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Addresses {
    pub nodes: IndexMap<String, Allocation>,
    pub gateway: Allocation,
}

fn err(msg: String) -> ModelError {
    ModelError(msg)
}

/// Allocates addresses for every node, keeping `previous` allocations where possible.
pub fn allocate(topo: &Topology, previous: Option<&Addresses>) -> Result<Addresses, ModelError> {
    let subnet = topo.subnet;
    if subnet.prefix() < 17 {
        return Err(err(format!(
            "`subnet` {subnet} is too wide: use /17 or longer (node indices must fit tc's 16-bit space)"
        )));
    }
    if topo.subnet6.prefix() > 112 {
        return Err(err(format!(
            "`subnet6` {} is too narrow: use /112 or wider",
            topo.subnet6
        )));
    }
    let gateway_index = subnet.last_usable_index();
    let usable = 1..gateway_index;

    let mut taken = BTreeSet::new();
    let mut indices: IndexMap<&str, u32> = IndexMap::new();

    for (node, spec) in &topo.nodes {
        let Some(addr) = spec.addr else { continue };
        let index = subnet.index_of(addr).ok_or_else(|| {
            err(format!(
                "node `{node}`: `addr` {addr} is outside `subnet` {subnet}"
            ))
        })?;
        if index == gateway_index {
            return Err(err(format!(
                "node `{node}`: `addr` {addr} is reserved for the gateway"
            )));
        }
        if !usable.contains(&index) {
            return Err(err(format!(
                "node `{node}`: `addr` {addr} is not a usable host address in {subnet}"
            )));
        }
        if !taken.insert(index) {
            return Err(err(format!(
                "node `{node}`: `addr` {addr} is already used by another node"
            )));
        }
        indices.insert(node, index);
    }

    if let Some(prev) = previous {
        for node in topo.nodes.keys() {
            if indices.contains_key(node.as_str()) {
                continue;
            }
            if let Some(alloc) = prev.nodes.get(node)
                && usable.contains(&alloc.index)
                && subnet.host(alloc.index) == Some(alloc.v4)
                && taken.insert(alloc.index)
            {
                indices.insert(node, alloc.index);
            }
        }
    }

    let mut next = usable.clone();
    for node in topo.nodes.keys() {
        if indices.contains_key(node.as_str()) {
            continue;
        }
        let index = next.by_ref().find(|i| !taken.contains(i)).ok_or_else(|| {
            err(format!(
                "`subnet` {subnet} has no room for {} nodes",
                topo.nodes.len()
            ))
        })?;
        taken.insert(index);
        indices.insert(node, index);
    }

    let mut nodes = IndexMap::new();
    let mut used6 = BTreeSet::new();
    for (node, spec) in &topo.nodes {
        let index = indices[node.as_str()];
        let v6 = match spec.addr6 {
            Some(a) if !topo.subnet6.contains(a) => {
                return Err(err(format!(
                    "node `{node}`: `addr6` {a} is outside `subnet6` {}",
                    topo.subnet6
                )));
            }
            Some(a) => a,
            None => topo.subnet6.host(index).expect("subnet6 is at least /112"),
        };
        if !used6.insert(v6) {
            return Err(err(format!(
                "node `{node}`: IPv6 address {v6} is already used by another node"
            )));
        }
        let v4 = subnet.host(index).expect("index is inside the subnet");
        nodes.insert(node.clone(), Allocation { index, v4, v6 });
    }

    let gateway = Allocation {
        index: gateway_index,
        v4: subnet
            .host(gateway_index)
            .expect("gateway index is inside the subnet"),
        v6: topo
            .subnet6
            .host(gateway_index)
            .expect("subnet6 is at least /112"),
    };
    if used6.contains(&gateway.v6) {
        return Err(err(format!(
            "IPv6 address {} is reserved for the gateway",
            gateway.v6
        )));
    }
    Ok(Addresses { nodes, gateway })
}

/// A MAC address.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Mac(pub [u8; 6]);

impl fmt::Display for Mac {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let [a, b, c, d, e, g] = self.0;
        write!(f, "{a:02x}:{b:02x}:{c:02x}:{d:02x}:{e:02x}:{g:02x}")
    }
}

/// A stable 16-bit hash of the lab name (FNV-1a, folded).
pub fn lab_hash(name: &str) -> u16 {
    let mut h: u32 = 0x811c_9dc5;
    for b in name.bytes() {
        h ^= u32::from(b);
        h = h.wrapping_mul(0x0100_0193);
    }
    ((h >> 16) ^ (h & 0xffff)) as u16
}

/// `02:74:<hash>:<index>`: the MAC of a node's (or the gateway's) interface.
pub fn mac(hash: u16, index: u32) -> Mac {
    let [h1, h2] = hash.to_be_bytes();
    let [_, _, i1, i2] = index.to_be_bytes();
    Mac([0x02, 0x74, h1, h2, i1, i2])
}

/// The MAC of node `own`'s routed-mode veth towards `peer`.
pub fn routed_mac(hash: u16, peer: u32, own: u32) -> Mac {
    mac(hash.wrapping_add(peer as u16), own)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Topology;

    fn topo(src: &str) -> Topology {
        Topology::parse(src, "lab").unwrap()
    }

    fn v4(a: &Addresses, node: &str) -> String {
        a.nodes[node].v4.to_string()
    }

    #[test]
    fn allocates_in_declaration_order_skipping_explicit_addresses() {
        let t = topo("[nodes]\na={}\nb={ addr = \"10.77.0.2\" }\nc={}\n");
        let a = allocate(&t, None).unwrap();
        assert_eq!(v4(&a, "a"), "10.77.0.1");
        assert_eq!(v4(&a, "b"), "10.77.0.2");
        assert_eq!(v4(&a, "c"), "10.77.0.3");
        assert_eq!(a.nodes["c"].index, 3);
        assert_eq!(a.nodes["c"].v6.to_string(), "fd77::3");
    }

    #[test]
    fn reserves_the_last_usable_address_for_the_gateway() {
        let t = topo("[nodes]\na={}\n");
        let a = allocate(&t, None).unwrap();
        assert_eq!(a.gateway.v4.to_string(), "10.77.0.254");
        assert_eq!(a.gateway.v6.to_string(), "fd77::fe");
        let err = allocate(&topo("[nodes]\na={ addr = \"10.77.0.254\" }\n"), None).unwrap_err();
        assert!(err.to_string().contains("gateway"), "{err}");
    }

    #[test]
    fn explicit_addr6_overrides_the_derived_one() {
        let t = topo("[nodes]\na={ addr6 = \"fd77::99\" }\n");
        assert_eq!(
            allocate(&t, None).unwrap().nodes["a"].v6.to_string(),
            "fd77::99"
        );
        let err = allocate(&topo("[nodes]\na={ addr6 = \"fd99::1\" }\n"), None).unwrap_err();
        assert!(err.to_string().contains("subnet6"), "{err}");
    }

    #[test]
    fn rejects_addresses_outside_the_subnet_and_clashes() {
        let err = allocate(&topo("[nodes]\na={ addr = \"10.78.0.1\" }\n"), None).unwrap_err();
        assert!(err.to_string().contains("10.78.0.1"), "{err}");
        let err = allocate(&topo("[nodes]\na={ addr = \"10.77.0.0\" }\n"), None).unwrap_err();
        assert!(err.to_string().contains("10.77.0.0"), "{err}");
        let err = allocate(
            &topo("[nodes]\na={ addr = \"10.77.0.5\" }\nb={ addr = \"10.77.0.5\" }\n"),
            None,
        )
        .unwrap_err();
        assert!(err.to_string().contains("10.77.0.5"), "{err}");
    }

    #[test]
    fn rejects_subnets_that_are_too_small_or_too_wide() {
        let err = allocate(
            &topo("subnet = \"10.77.0.0/30\"\n[nodes]\na={}\nb={}\n"),
            None,
        )
        .unwrap_err();
        assert!(err.to_string().contains("room"), "{err}");
        let err = allocate(&topo("subnet = \"10.0.0.0/16\"\n[nodes]\na={}\n"), None).unwrap_err();
        assert!(err.to_string().contains("/17"), "{err}");
    }

    #[test]
    fn keeps_previous_allocations_across_apply() {
        let before = allocate(&topo("[nodes]\na={}\nb={}\nc={}\n"), None).unwrap();
        let after = allocate(&topo("[nodes]\na={}\nc={}\nd={}\n"), Some(&before)).unwrap();
        assert_eq!(v4(&after, "a"), "10.77.0.1");
        assert_eq!(v4(&after, "c"), "10.77.0.3");
        assert_eq!(
            v4(&after, "d"),
            "10.77.0.2",
            "new nodes take the lowest free address"
        );
    }

    #[test]
    fn explicit_addresses_win_over_previous_allocations() {
        let before = allocate(&topo("[nodes]\na={}\nb={}\n"), None).unwrap();
        let after = allocate(
            &topo("[nodes]\na={}\nb={ addr = \"10.77.0.1\" }\n"),
            Some(&before),
        )
        .unwrap();
        assert_eq!(v4(&after, "b"), "10.77.0.1");
        assert_eq!(v4(&after, "a"), "10.77.0.2");
    }

    #[test]
    fn macs_are_deterministic_and_locally_administered() {
        let h = lab_hash("chain");
        assert_eq!(h, lab_hash("chain"));
        assert_ne!(h, lab_hash("other"));
        let m = mac(0x1234, 5);
        assert_eq!(m.to_string(), "02:74:12:34:00:05");
        assert_eq!(routed_mac(0xffff, 2, 7).to_string(), "02:74:00:01:00:07");
    }
}
