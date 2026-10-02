//! Direct mode (spec §4.1): one bridge, shaping by destination MAC, reachability by a
//! source-MAC allowlist at the receiver.

use super::{EffEdge, Op, Target};
use crate::addr::{Addresses, Allocation, Mac, lab_hash, mac};
use crate::model::{Edge, INTERNET, LinkProps, Topology};
use crate::units::tc_duration;
use indexmap::IndexMap;
use std::collections::BTreeMap;
use std::net::{Ipv4Addr, Ipv6Addr};

/// Everything planning needs besides the effective edges.
pub struct Ctx<'a> {
    pub topo: &'a Topology,
    pub addrs: &'a Addresses,
    pub hash: u16,
}

impl<'a> Ctx<'a> {
    pub fn new(topo: &'a Topology, addrs: &'a Addresses) -> Self {
        Self {
            topo,
            addrs,
            hash: lab_hash(&topo.name),
        }
    }

    fn alloc(&self, endpoint: &str) -> Allocation {
        if endpoint == INTERNET {
            self.addrs.gateway
        } else {
            self.addrs.nodes[endpoint]
        }
    }

    fn iface(&self, endpoint: &str) -> Iface {
        let a = self.alloc(endpoint);
        Iface {
            index: a.index,
            v4: a.v4,
            v6: a.v6,
            mac: mac(self.hash, a.index),
            prefix4: self.topo.subnet.prefix(),
            prefix6: self.topo.subnet6.prefix(),
        }
    }

    fn target(endpoint: &str) -> Target {
        if endpoint == INTERNET {
            Target::Gateway
        } else {
            Target::node(endpoint)
        }
    }

    fn endpoints(&self) -> Vec<&str> {
        let mut out: Vec<&str> = self.topo.nodes.keys().map(String::as_str).collect();
        if self.topo.uses_internet() {
            out.push(INTERNET);
        }
        out
    }
}

/// An endpoint's interface on the bridge.
#[derive(Clone, Debug, PartialEq)]
pub struct Iface {
    pub index: u32,
    pub v4: Ipv4Addr,
    pub v6: Ipv6Addr,
    pub mac: Mac,
    pub prefix4: u8,
    pub prefix6: u8,
}

impl Iface {
    fn link_local(&self) -> String {
        format!("fe80::{:x}", self.index)
    }
}

/// The desired network state of one endpoint's `eth0`. Keys are peer indices.
#[derive(Clone, Debug, PartialEq)]
pub struct NodeNet {
    pub own: Iface,
    /// Egress: one HTB class + netem per peer this endpoint has an edge to.
    pub classes: BTreeMap<u32, (Mac, LinkProps)>,
    /// Ingress: peers with a live edge to this endpoint.
    pub allow: BTreeMap<u32, Mac>,
    /// Static neighbour entries for every other endpoint.
    pub neighbours: BTreeMap<u32, Iface>,
    /// The gateway's addresses, when this node has a live edge to `internet`.
    pub default_route: Option<(Ipv4Addr, Ipv6Addr)>,
}

pub type Desired = IndexMap<Target, NodeNet>;

pub fn desired(ctx: &Ctx, eff: &IndexMap<Edge, EffEdge>) -> Desired {
    let endpoints = ctx.endpoints();
    let gateway = ctx.iface(INTERNET);
    endpoints
        .iter()
        .map(|&me| {
            let mut classes = BTreeMap::new();
            let mut allow = BTreeMap::new();
            for (edge, e) in eff {
                if edge.from == me {
                    let peer = ctx.iface(&edge.to);
                    classes.insert(peer.index, (peer.mac, e.props.clone()));
                }
                if edge.to == me && e.up {
                    let peer = ctx.iface(&edge.from);
                    allow.insert(peer.index, peer.mac);
                }
            }
            let neighbours = endpoints
                .iter()
                .filter(|&&other| other != me)
                .map(|&other| {
                    let i = ctx.iface(other);
                    (i.index, i)
                })
                .collect();
            let to_internet = Edge::new(me, INTERNET);
            let default_route = eff
                .get(&to_internet)
                .is_some_and(|e| e.up)
                .then_some((gateway.v4, gateway.v6));
            (
                Ctx::target(me),
                NodeNet {
                    own: ctx.iface(me),
                    classes,
                    allow,
                    neighbours,
                    default_route,
                },
            )
        })
        .collect()
}

const DEV: &str = "eth0";
/// HTB is only a classifier here (netem does the shaping), so every class gets the same huge
/// rate; an explicit quantum avoids the kernel's "quantum is big" warning.
const CLASS: &str = "htb rate 10gbit ceil 10gbit quantum 200000";
const DEFAULT_CLASS: &str = "fffe";
const DROP_PREF: u32 = 65535;

/// netem arguments for `props` (everything after `netem`).
pub fn netem_args(props: &LinkProps) -> Vec<String> {
    let mut args = vec!["limit".to_owned(), netem_limit(props).to_string()];
    if let Some(d) = props.delay.filter(|d| !d.is_zero()) {
        args.extend(["delay".to_owned(), tc_duration(d)]);
        if let Some(j) = props.jitter.filter(|j| !j.is_zero()) {
            args.push(tc_duration(j));
        }
    }
    for (key, value) in [
        ("loss", props.loss),
        ("duplicate", props.duplicate),
        ("reorder", props.reorder),
        ("corrupt", props.corrupt),
    ] {
        if let Some(v) = value.filter(|v| !v.is_zero()) {
            args.extend([key.to_owned(), v.to_tc()]);
        }
    }
    if let Some(r) = props.rate.filter(|r| r.0 > 0) {
        args.extend(["rate".to_owned(), r.to_tc()]);
    }
    args
}

/// Twice the bandwidth-delay product in full-size packets, and at least 10 000.
fn netem_limit(props: &LinkProps) -> u64 {
    const MIN: u64 = 10_000;
    let (Some(delay), Some(rate)) = (props.delay, props.rate) else {
        return MIN;
    };
    let bytes = rate.0 as f64 / 8.0 * delay.as_secs_f64();
    MIN.max(((bytes / 1500.0).ceil() as u64) * 2)
}

fn neighbour_ops(t: &Target, n: &Iface, add: bool) -> Vec<Op> {
    if add {
        let mac = n.mac;
        vec![
            Op::ip(
                t,
                format!(
                    "neigh replace {} lladdr {mac} dev {DEV} nud permanent",
                    n.v4
                ),
            ),
            Op::ip(
                t,
                format!(
                    "neigh replace {} lladdr {mac} dev {DEV} nud permanent",
                    n.v6
                ),
            ),
            Op::ip(
                t,
                format!(
                    "neigh replace {} lladdr {mac} dev {DEV} nud permanent",
                    n.link_local()
                ),
            ),
        ]
    } else {
        vec![
            Op::ip(t, format!("neigh del {} dev {DEV}", n.v4)),
            Op::ip(t, format!("neigh del {} dev {DEV}", n.v6)),
            Op::ip(t, format!("neigh del {} dev {DEV}", n.link_local())),
        ]
    }
}

fn add_class(t: &Target, idx: u32, mac: Mac, props: &LinkProps) -> Vec<Op> {
    vec![
        Op::tc(
            t,
            format!("class add dev {DEV} parent 1: classid 1:{idx:x} {CLASS}"),
        ),
        Op::tc(
            t,
            format!(
                "qdisc replace dev {DEV} parent 1:{idx:x} netem {}",
                netem_args(props).join(" ")
            ),
        ),
        Op::tc(
            t,
            format!(
                "filter add dev {DEV} parent 1: pref {idx} protocol all flower dst_mac {mac} classid 1:{idx:x}"
            ),
        ),
    ]
}

fn allow_op(t: &Target, idx: u32, mac: Mac) -> Op {
    Op::tc(
        t,
        format!(
            "filter add dev {DEV} ingress pref {idx} protocol all flower src_mac {mac} action pass"
        ),
    )
}

/// The tc roots: an HTB classifier with an unshaped default class, and a default-drop ingress.
fn base_tc(t: &Target) -> Vec<Op> {
    vec![
        Op::tc(
            t,
            format!("qdisc add dev {DEV} root handle 1: htb default {DEFAULT_CLASS}"),
        ),
        Op::tc(
            t,
            format!("class add dev {DEV} parent 1: classid 1:{DEFAULT_CLASS} {CLASS}"),
        ),
        Op::tc(t, format!("qdisc add dev {DEV} clsact")),
        Op::tc(
            t,
            format!(
                "filter add dev {DEV} ingress pref {DROP_PREF} protocol all matchall action drop"
            ),
        ),
    ]
}

/// Clears an endpoint's tc, neighbour and default-route state back to the bare tc roots.
pub fn reset_ops(t: &Target) -> Vec<Op> {
    let mut ops = vec![
        Op::tc(t, format!("qdisc del dev {DEV} root")),
        Op::tc(t, format!("qdisc del dev {DEV} clsact")),
        Op::ip(t, format!("neigh flush dev {DEV}")),
        Op::ip(t, "route flush exact 0.0.0.0/0"),
        Op::ip(t, "route flush exact ::/0"),
    ];
    ops.extend(base_tc(t));
    ops
}

/// After [`reset_ops`]: everything that makes up `net`.
pub fn diff_from_reset(t: &Target, net: &NodeNet) -> Vec<Op> {
    diff_node(t, None, net)
}

/// The full set of operations that produce `net` from an empty namespace (`tpo explain`).
pub fn explain_ops(t: &Target, net: &NodeNet) -> Vec<Op> {
    let mut ops = setup_ops(t, &net.own);
    ops.extend(diff_node(t, None, net));
    ops
}

/// Creates an endpoint's interface, addresses and tc roots.
fn setup_ops(t: &Target, own: &Iface) -> Vec<Op> {
    let mut ops = vec![
        Op::sysctl(t, "net.ipv6.conf.all.accept_dad", "0"),
        Op::sysctl(t, "net.ipv6.conf.default.accept_dad", "0"),
        Op::sysctl(t, "net.ipv4.conf.all.rp_filter", "0"),
        Op::sysctl(t, "net.ipv4.conf.default.rp_filter", "0"),
        Op::ip(t, "link set lo up"),
        Op::ip(
            t,
            format!(
                "link add {DEV} address {} type veth peer name v{} netns {}",
                own.mac,
                own.index,
                super::ops::HUB_PID
            ),
        ),
        Op::ip(t, format!("link set {DEV} addrgenmode none")),
        Op::ip(t, format!("addr add {}/{} dev {DEV}", own.v4, own.prefix4)),
        Op::ip(
            t,
            format!("addr add {}/{} dev {DEV} nodad", own.v6, own.prefix6),
        ),
        Op::ip(
            t,
            format!("addr add {}/64 dev {DEV} nodad", own.link_local()),
        ),
        Op::ip(
            &Target::Hub,
            format!("link set v{} master br0 up", own.index),
        ),
        Op::ip(t, format!("link set {DEV} up")),
    ];
    ops.extend(base_tc(t));
    if *t == Target::Gateway {
        ops.push(Op::sysctl(t, "net.ipv4.ip_forward", "1"));
        ops.push(Op::sysctl(t, "net.ipv6.conf.all.forwarding", "1"));
    }
    ops
}

fn diff_node(t: &Target, old: Option<&NodeNet>, new: &NodeNet) -> Vec<Op> {
    let mut ops = Vec::new();
    let empty = BTreeMap::new();
    let empty_n = BTreeMap::new();
    let empty_a = BTreeMap::new();
    let (old_classes, old_allow, old_neigh, old_route) = match old {
        Some(o) => (&o.classes, &o.allow, &o.neighbours, o.default_route),
        None => (&empty, &empty_a, &empty_n, None),
    };

    for (idx, n) in &new.neighbours {
        if old_neigh.get(idx) != Some(n) {
            ops.extend(neighbour_ops(t, n, true));
        }
    }
    for (idx, n) in old_neigh {
        if !new.neighbours.contains_key(idx) {
            ops.extend(neighbour_ops(t, n, false));
        }
    }

    for (idx, (mac, props)) in &new.classes {
        match old_classes.get(idx) {
            None => ops.extend(add_class(t, *idx, *mac, props)),
            Some((old_mac, old_props)) if old_mac == mac => {
                if old_props != props && netem_args(old_props) != netem_args(props) {
                    ops.push(Op::tc(
                        t,
                        format!(
                            "qdisc replace dev {DEV} parent 1:{idx:x} netem {}",
                            netem_args(props).join(" ")
                        ),
                    ));
                }
            }
            Some(_) => {
                ops.push(Op::tc(
                    t,
                    format!("filter del dev {DEV} parent 1: pref {idx}"),
                ));
                ops.push(Op::tc(t, format!("class del dev {DEV} classid 1:{idx:x}")));
                ops.extend(add_class(t, *idx, *mac, props));
            }
        }
    }
    for idx in old_classes.keys() {
        if !new.classes.contains_key(idx) {
            ops.push(Op::tc(
                t,
                format!("filter del dev {DEV} parent 1: pref {idx}"),
            ));
            ops.push(Op::tc(t, format!("class del dev {DEV} classid 1:{idx:x}")));
        }
    }

    for (idx, mac) in &new.allow {
        match old_allow.get(idx) {
            Some(old_mac) if old_mac == mac => {}
            Some(_) => {
                ops.push(Op::tc(
                    t,
                    format!("filter del dev {DEV} ingress pref {idx}"),
                ));
                ops.push(allow_op(t, *idx, *mac));
            }
            None => ops.push(allow_op(t, *idx, *mac)),
        }
    }
    for idx in old_allow.keys() {
        if !new.allow.contains_key(idx) {
            ops.push(Op::tc(
                t,
                format!("filter del dev {DEV} ingress pref {idx}"),
            ));
        }
    }

    match (old_route, new.default_route) {
        (old, Some((v4, v6))) if old != Some((v4, v6)) => {
            ops.push(Op::ip(
                t,
                format!("route replace default via {v4} dev {DEV}"),
            ));
            ops.push(Op::ip(t, format!("route replace ::/0 via {v6} dev {DEV}")));
        }
        (Some(_), None) => {
            ops.push(Op::ip(t, "route del default"));
            ops.push(Op::ip(t, "route del ::/0"));
        }
        _ => {}
    }
    ops
}

/// Operations from `old` to `new`. Endpoints only in `new` are created; endpoints only in `old`
/// need no operations (their namespaces are destroyed, taking their interfaces with them).
pub fn diff(old: &Desired, new: &Desired) -> Vec<Op> {
    // Namespaces are destroyed asynchronously, so remove a gone endpoint's bridge port first:
    // a new endpoint may reuse its index (and so its port name) in this same diff.
    let mut ops: Vec<Op> = old
        .iter()
        .filter(|(t, _)| !new.contains_key(*t))
        .map(|(_, net)| Op::ip(&Target::Hub, format!("link del v{}", net.own.index)))
        .collect();
    for (t, net) in new {
        match old.get(t) {
            Some(o) if o.own == net.own => ops.extend(diff_node(t, Some(o), net)),
            _ => {
                ops.extend(setup_ops(t, &net.own));
                ops.extend(diff_node(t, None, net));
            }
        }
    }
    ops
}

/// Everything needed to build a lab from empty namespaces.
pub fn initial_ops(desired: &Desired) -> Vec<Op> {
    let hub = Target::Hub;
    let mut ops = vec![
        Op::ip(&hub, "link set lo up"),
        Op::ip(&hub, "link add br0 type bridge mcast_snooping 0"),
        Op::ip(&hub, "link set br0 up"),
    ];
    ops.extend(diff(&Desired::new(), desired));
    ops
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::addr::allocate;
    use crate::model::{Edge, Topology};
    use crate::plan::{Faults, Until, effective, parse_assignments};

    fn topo(src: &str) -> Topology {
        Topology::parse(src, "lab").unwrap()
    }

    const CHAIN: &str =
        "[nodes]\na={}\nb={}\nc={}\n[[link]]\npath=[\"a\",\"b\",\"c\"]\ndelay=\"10ms\"\n";

    fn plan(t: &Topology, f: &Faults) -> Desired {
        let addrs = allocate(t, None).unwrap();
        desired(&Ctx::new(t, &addrs), &effective(t, f, 0))
    }

    fn rendered(ops: &[Op]) -> Vec<String> {
        ops.iter().map(ToString::to_string).collect()
    }

    fn mac_of(t: &Topology, index: u32) -> String {
        crate::addr::mac(crate::addr::lab_hash(&t.name), index).to_string()
    }

    #[test]
    fn nodes_shape_towards_and_accept_from_their_neighbours_only() {
        let t = topo(CHAIN);
        let d = plan(&t, &Faults::default());
        let a = &d[&Target::node("a")];
        assert_eq!(a.classes.keys().copied().collect::<Vec<_>>(), [2]);
        assert_eq!(a.allow.keys().copied().collect::<Vec<_>>(), [2]);
        let b = &d[&Target::node("b")];
        assert_eq!(b.classes.keys().copied().collect::<Vec<_>>(), [1, 3]);
        assert_eq!(b.allow.keys().copied().collect::<Vec<_>>(), [1, 3]);
        assert!(!d.contains_key(&Target::Gateway));
    }

    #[test]
    fn every_node_has_static_neighbours_for_every_other_node() {
        let t = topo(CHAIN);
        let d = plan(&t, &Faults::default());
        assert_eq!(
            d[&Target::node("a")]
                .neighbours
                .keys()
                .copied()
                .collect::<Vec<_>>(),
            [2, 3]
        );
    }

    #[test]
    fn initial_ops_create_interfaces_and_per_peer_tc_state() {
        let t = topo(CHAIN);
        let addrs = allocate(&t, None).unwrap();
        let ctx = Ctx::new(&t, &addrs);
        let ops = rendered(&initial_ops(&desired(
            &ctx,
            &effective(&t, &Faults::default(), 0),
        )));
        let b_mac = mac_of(&t, 2);
        let a_mac = mac_of(&t, 1);
        for expected in [
            "hub: ip link add br0 type bridge mcast_snooping 0".to_owned(),
            format!("a: ip link add eth0 address {a_mac} type veth peer name v1 netns @hub"),
            "a: ip addr add 10.77.0.1/24 dev eth0".to_owned(),
            "a: ip addr add fd77::1/64 dev eth0 nodad".to_owned(),
            "a: ip addr add fe80::1/64 dev eth0 nodad".to_owned(),
            "hub: ip link set v1 master br0 up".to_owned(),
            format!("a: ip neigh replace 10.77.0.2 lladdr {b_mac} dev eth0 nud permanent"),
            format!("a: ip neigh replace fd77::2 lladdr {b_mac} dev eth0 nud permanent"),
            format!("a: ip neigh replace fe80::2 lladdr {b_mac} dev eth0 nud permanent"),
            "a: tc qdisc add dev eth0 root handle 1: htb default fffe".to_owned(),
            "a: tc filter add dev eth0 ingress pref 65535 protocol all matchall action drop".to_owned(),
            "a: tc class add dev eth0 parent 1: classid 1:2 htb rate 10gbit ceil 10gbit quantum 200000".to_owned(),
            "a: tc qdisc replace dev eth0 parent 1:2 netem limit 10000 delay 10000us".to_owned(),
            format!("a: tc filter add dev eth0 parent 1: pref 2 protocol all flower dst_mac {b_mac} classid 1:2"),
            format!("a: tc filter add dev eth0 ingress pref 2 protocol all flower src_mac {b_mac} action pass"),
        ] {
            assert!(ops.contains(&expected), "missing `{expected}` in:\n{}", ops.join("\n"));
        }
        let veth = ops
            .iter()
            .position(|o| o.starts_with("a: ip link add eth0"))
            .unwrap();
        let port = ops
            .iter()
            .position(|o| o == "hub: ip link set v1 master br0 up")
            .unwrap();
        assert!(
            veth < port,
            "the hub port is configured after the veth exists"
        );
        assert!(
            !ops.iter()
                .any(|o| o.starts_with("a:") && o.contains("pref 3 ")),
            "a has no link to c"
        );
    }

    #[test]
    fn class_ids_are_hex_and_prefs_decimal() {
        let t = topo("[nodes]\na={}\nb={ addr = \"10.77.0.20\" }\n[[link]]\npath=[\"a\",\"b\"]\n");
        let addrs = allocate(&t, None).unwrap();
        let ctx = Ctx::new(&t, &addrs);
        let ops = rendered(&initial_ops(&desired(
            &ctx,
            &effective(&t, &Faults::default(), 0),
        )));
        assert!(
            ops.iter()
                .any(|o| o.starts_with("a: tc class add") && o.contains("classid 1:14 "))
        );
        assert!(
            ops.iter()
                .any(|o| o.starts_with("a: tc filter add dev eth0 parent 1: pref 20 "))
        );
    }

    #[test]
    fn cutting_one_direction_removes_one_allow_rule_at_the_receiver() {
        let t = topo(CHAIN);
        let before = plan(&t, &Faults::default());
        let mut f = Faults::default();
        f.cut(&[Edge::new("a", "b")], Until::FOREVER);
        let after = plan(&t, &f);
        assert_eq!(
            rendered(&diff(&before, &after)),
            ["b: tc filter del dev eth0 ingress pref 1"]
        );
        assert_eq!(
            rendered(&diff(&after, &before)),
            [format!(
                "b: tc filter add dev eth0 ingress pref 1 protocol all flower src_mac {} action pass",
                mac_of(&t, 1)
            )]
        );
    }

    #[test]
    fn changing_properties_replaces_only_that_peers_netem() {
        let t = topo(CHAIN);
        let before = plan(&t, &Faults::default());
        let mut f = Faults::default();
        let a = parse_assignments(&["delay=200ms", "loss=5%", "rate=1mbit"]).unwrap();
        f.set(&t, &[Edge::new("a", "b")], &a, Until::FOREVER)
            .unwrap();
        let after = plan(&t, &f);
        assert_eq!(
            rendered(&diff(&before, &after)),
            [
                "a: tc qdisc replace dev eth0 parent 1:2 netem limit 10000 delay 200000us loss 5% rate 1000000bit"
            ]
        );
    }

    #[test]
    fn long_fat_links_get_a_bigger_netem_queue() {
        let props = LinkProps {
            delay: Some(std::time::Duration::from_millis(200)),
            rate: Some(crate::units::Rate(1_000_000_000)),
            ..LinkProps::default()
        };
        // 1 Gbit/s × 200 ms = 25 MB ≈ 16_667 full-size packets; twice that is the limit.
        assert_eq!(
            netem_args(&props)[..2],
            ["limit".to_owned(), "33334".to_owned()]
        );
    }

    #[test]
    fn removed_endpoints_lose_their_bridge_port_and_neighbour_entries() {
        let before = plan(&topo(CHAIN), &Faults::default());
        let smaller = topo("[nodes]\na={}\nb={}\n[[link]]\npath=[\"a\",\"b\"]\ndelay=\"10ms\"\n");
        let after = plan(&smaller, &Faults::default());
        let ops = rendered(&diff(&before, &after));
        assert!(ops.contains(&"hub: ip link del v3".to_owned()), "{ops:?}");
        assert!(
            ops.contains(&"b: ip neigh del 10.77.0.3 dev eth0".to_owned()),
            "{ops:?}"
        );
    }

    #[test]
    fn internet_edges_give_nodes_a_default_route_and_the_gateway_an_allowlist() {
        let t =
            topo("[nodes]\na={}\nb={}\n[[link]]\nfrom=\"a\"\nto=\"internet\"\ndelay=\"30ms\"\n");
        let d = plan(&t, &Faults::default());
        let a = &d[&Target::node("a")];
        assert!(a.default_route.is_some());
        assert_eq!(a.classes.keys().copied().collect::<Vec<_>>(), [254]);
        assert!(d[&Target::node("b")].default_route.is_none());
        let gw = &d[&Target::Gateway];
        assert_eq!(gw.allow.keys().copied().collect::<Vec<_>>(), [1]);
        assert_eq!(gw.classes.keys().copied().collect::<Vec<_>>(), [1]);
        assert_eq!(
            d[&Target::node("b")]
                .neighbours
                .keys()
                .copied()
                .collect::<Vec<_>>(),
            [1, 254]
        );

        let mut f = Faults::default();
        f.cut(
            &[Edge::new("a", "internet"), Edge::new("internet", "a")],
            Until::FOREVER,
        );
        let ops = rendered(&diff(&d, &plan(&t, &f)));
        assert!(
            ops.contains(&"a: ip route del default".to_owned()),
            "{ops:?}"
        );
        assert!(ops.contains(&"a: ip route del ::/0".to_owned()), "{ops:?}");
        assert!(
            ops.contains(&"gateway: tc filter del dev eth0 ingress pref 1".to_owned()),
            "{ops:?}"
        );
    }
}
