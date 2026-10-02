//! Routed mode (spec §4.2): a veth per link, `/32`/`/128` node addresses on `lo`, static
//! shortest-path routes, cuts as per-direction drops at the receiving veth.

use super::direct::{Ctx, Iface, netem_args};
use super::{EffEdge, Op, Target};
use crate::addr::{Mac, routed_mac};
use crate::model::{Edge, INTERNET, LinkProps, Metric};
use indexmap::IndexMap;
use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap};

/// One end of a link's veth pair, named `p<peer index>`.
#[derive(Clone, Debug, PartialEq)]
pub struct Port {
    pub peer: Iface,
    /// The peer's target, for naming its namespace when creating the pair.
    pub peer_target: Target,
    pub own_mac: Mac,
    pub peer_mac: Mac,
    /// Properties of the edge towards the peer (no-op when there is no such edge).
    pub egress: LinkProps,
    /// Whether the edge from the peer exists and is up; otherwise its traffic is dropped here.
    pub accept: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Route {
    pub dst: Iface,
    /// The neighbour (port) packets for `dst` leave through.
    pub via: u32,
    pub via_iface: Iface,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RoutedNet {
    pub own: Iface,
    pub ports: BTreeMap<u32, Port>,
    pub routes: BTreeMap<u32, Route>,
    /// Towards `internet`: the neighbour, if this is a node with a path to it.
    pub default_via: Option<(u32, Iface)>,
}

pub type Desired = IndexMap<Target, RoutedNet>;

fn weight(metric: Metric, props: &LinkProps) -> u64 {
    match metric {
        Metric::Hops => 1,
        Metric::Delay => props.delay.map_or(1, |d| d.as_micros().max(1) as u64),
    }
}

/// First hops from `src` to every reachable endpoint over live edges. `internet` is never a
/// transit hop (the gateway drops lab→lab traffic).
fn first_hops<'a>(
    src: &'a str,
    eff: &'a IndexMap<Edge, EffEdge>,
    metric: Metric,
) -> BTreeMap<&'a str, &'a str> {
    let mut dist: BTreeMap<&str, u64> = BTreeMap::new();
    let mut first: BTreeMap<&str, &str> = BTreeMap::new();
    let mut heap = BinaryHeap::new();
    dist.insert(src, 0);
    heap.push(Reverse((0u64, src, src)));
    while let Some(Reverse((d, node, hop))) = heap.pop() {
        if dist.get(node).is_some_and(|&best| d > best) {
            continue;
        }
        if node != src {
            first.entry(node).or_insert(hop);
        }
        if node == INTERNET && node != src {
            continue;
        }
        for (edge, e) in eff.iter().filter(|(edge, e)| edge.from == node && e.up) {
            let next = edge.to.as_str();
            let nd = d + weight(metric, &e.props);
            if dist.get(next).is_none_or(|&best| nd < best) {
                dist.insert(next, nd);
                let hop = if node == src { next } else { hop };
                first.insert(next, hop);
                heap.push(Reverse((nd, next, hop)));
            }
        }
    }
    first
}

pub fn desired(ctx: &Ctx, eff: &IndexMap<Edge, EffEdge>, prev: Option<&Desired>) -> Desired {
    let mut out = Desired::new();
    for me in ctx.endpoints() {
        let own = ctx.iface(me);
        let target = Ctx::target(me);
        let mut ports = BTreeMap::new();
        for edge in eff.keys() {
            let peer = if edge.from == me {
                &edge.to
            } else if edge.to == me {
                &edge.from
            } else {
                continue;
            };
            let p = ctx.iface(peer);
            ports.entry(p.index).or_insert_with(|| {
                let out_edge = eff.get(&Edge::new(me, peer.as_str()));
                let in_edge = eff.get(&Edge::new(peer.as_str(), me));
                Port {
                    peer_target: Ctx::target(peer),
                    own_mac: routed_mac(ctx.hash, p.index, own.index),
                    peer_mac: routed_mac(ctx.hash, own.index, p.index),
                    egress: out_edge.map(|e| e.props.clone()).unwrap_or_default(),
                    accept: in_edge.is_some_and(|e| e.up),
                    peer: p,
                }
            });
        }

        let hops = first_hops(me, eff, ctx.topo.metric);
        let previous = prev.and_then(|p| p.get(&target));
        let mut routes = BTreeMap::new();
        for other in ctx.endpoints() {
            if other == me || other == INTERNET {
                continue;
            }
            let dst = ctx.iface(other);
            let route = match hops.get(other) {
                Some(hop) => {
                    let via = ctx.iface(hop);
                    Some(Route {
                        dst: dst.clone(),
                        via: via.index,
                        via_iface: via,
                    })
                }
                // No path: keep the previous route so packets die at the cut (spec §4.2).
                None => previous
                    .and_then(|p| p.routes.get(&dst.index))
                    .filter(|r| ports.contains_key(&r.via))
                    .cloned(),
            };
            if let Some(r) = route {
                routes.insert(dst.index, r);
            }
        }

        let default_via = if me == INTERNET {
            None
        } else {
            match hops.get(INTERNET) {
                Some(hop) => {
                    let via = ctx.iface(hop);
                    Some((via.index, via))
                }
                None => previous
                    .and_then(|p| p.default_via.clone())
                    .filter(|(i, _)| ports.contains_key(i)),
            }
        };

        out.insert(
            target,
            RoutedNet {
                own,
                ports,
                routes,
                default_via,
            },
        );
    }
    out
}

const DROP_PREF: u32 = 1;

fn setup_ops(t: &Target, own: &Iface) -> Vec<Op> {
    vec![
        Op::sysctl(t, "net.ipv6.conf.all.accept_dad", "0"),
        Op::sysctl(t, "net.ipv6.conf.default.accept_dad", "0"),
        Op::sysctl(t, "net.ipv4.conf.all.rp_filter", "0"),
        Op::sysctl(t, "net.ipv4.conf.default.rp_filter", "0"),
        Op::sysctl(t, "net.ipv4.ip_forward", "1"),
        Op::sysctl(t, "net.ipv6.conf.all.forwarding", "1"),
        Op::ip(t, "link set lo up"),
        Op::ip(t, format!("addr add {}/32 dev lo", own.v4)),
        Op::ip(t, format!("addr add {}/128 dev lo", own.v6)),
    ]
}

fn create_port(t: &Target, port: &Port, own: &Iface) -> Op {
    Op::ip(
        t,
        format!(
            "link add p{} address {} type veth peer name p{} address {} netns @netns:{}",
            port.peer.index, port.own_mac, own.index, port.peer_mac, port.peer_target
        ),
    )
}

fn neighbour_ops(t: &Target, dev: &str, n: &Iface, mac: Mac) -> Vec<Op> {
    [n.v4.to_string(), n.v6.to_string(), n.link_local()]
        .into_iter()
        .map(|addr| {
            Op::ip(
                t,
                format!("neigh replace {addr} lladdr {mac} dev {dev} nud permanent"),
            )
        })
        .collect()
}

fn configure_port(t: &Target, port: &Port, own: &Iface) -> Vec<Op> {
    let dev = format!("p{}", port.peer.index);
    let mut ops = vec![
        Op::ip(t, format!("link set {dev} addrgenmode none")),
        Op::ip(
            t,
            format!("addr add {}/64 dev {dev} nodad", own.link_local()),
        ),
        Op::ip(t, format!("link set {dev} up")),
    ];
    ops.extend(neighbour_ops(t, &dev, &port.peer, port.peer_mac));
    ops.push(Op::tc(
        t,
        format!(
            "qdisc replace dev {dev} root netem {}",
            netem_args(&port.egress).join(" ")
        ),
    ));
    ops.push(Op::tc(t, format!("qdisc add dev {dev} clsact")));
    if !port.accept {
        ops.push(drop_op(t, &dev));
    }
    ops
}

fn drop_op(t: &Target, dev: &str) -> Op {
    Op::tc(
        t,
        format!("filter add dev {dev} ingress pref {DROP_PREF} protocol all matchall action drop"),
    )
}

fn route_ops(t: &Target, r: &Route) -> Vec<Op> {
    let dev = format!("p{}", r.via);
    if r.via == r.dst.index {
        vec![
            Op::ip(t, format!("route replace {}/32 dev {dev}", r.dst.v4)),
            Op::ip(t, format!("route replace {}/128 dev {dev}", r.dst.v6)),
        ]
    } else {
        vec![
            Op::ip(
                t,
                format!(
                    "route replace {}/32 via {} dev {dev} onlink",
                    r.dst.v4, r.via_iface.v4
                ),
            ),
            Op::ip(
                t,
                format!(
                    "route replace {}/128 via {} dev {dev}",
                    r.dst.v6,
                    r.via_iface.link_local()
                ),
            ),
        ]
    }
}

fn default_ops(t: &Target, via: &Option<(u32, Iface)>, old: &Option<(u32, Iface)>) -> Vec<Op> {
    match (old, via) {
        (o, Some((idx, iface))) if o.as_ref().map(|o| o.0) != Some(*idx) => vec![
            Op::ip(
                t,
                format!("route replace default via {} dev p{idx} onlink", iface.v4),
            ),
            Op::ip(
                t,
                format!("route replace ::/0 via {} dev p{idx}", iface.link_local()),
            ),
        ],
        (Some(_), None) => vec![Op::ip(t, "route del default"), Op::ip(t, "route del ::/0")],
        _ => vec![],
    }
}

/// Operations from `old` to `new`, in three phases: create veth pairs, configure ports, then
/// routes (a pair must exist on both sides before either side configures it).
pub fn diff(old: &Desired, new: &Desired) -> Vec<Op> {
    let mut creates = Vec::new();
    let mut deletes = Vec::new();
    let mut config = Vec::new();
    let mut routes = Vec::new();
    let empty = BTreeMap::new();

    for (t, net) in new {
        let prev = old.get(t).filter(|o| o.own == net.own);
        if prev.is_none() {
            config.extend(setup_ops(t, &net.own));
        }
        let old_ports = prev.map_or(&empty, |p| &p.ports);
        for (idx, port) in &net.ports {
            match old_ports.get(idx) {
                None => {
                    if net.own.index < *idx {
                        creates.push(create_port(t, port, &net.own));
                    }
                    config.extend(configure_port(t, port, &net.own));
                }
                Some(o) => {
                    let dev = format!("p{idx}");
                    if netem_args(&o.egress) != netem_args(&port.egress) {
                        config.push(Op::tc(
                            t,
                            format!(
                                "qdisc replace dev {dev} root netem {}",
                                netem_args(&port.egress).join(" ")
                            ),
                        ));
                    }
                    match (o.accept, port.accept) {
                        (true, false) => config.push(drop_op(t, &dev)),
                        (false, true) => config.push(Op::tc(
                            t,
                            format!("filter del dev {dev} ingress pref {DROP_PREF}"),
                        )),
                        _ => {}
                    }
                }
            }
        }
        for (idx, _) in old_ports
            .iter()
            .filter(|(i, _)| !net.ports.contains_key(*i))
        {
            let peer_stays = new.values().any(|n| n.own.index == *idx);
            if !peer_stays || net.own.index < *idx {
                deletes.push(Op::ip(t, format!("link del p{idx}")));
            }
        }

        let old_routes = prev.map(|p| &p.routes);
        for (dst, r) in &net.routes {
            if old_routes.and_then(|o| o.get(dst)) != Some(r) {
                routes.extend(route_ops(t, r));
            }
        }
        if let Some(old_routes) = old_routes {
            for (dst, r) in old_routes
                .iter()
                .filter(|(d, _)| !net.routes.contains_key(*d))
            {
                let _ = dst;
                routes.push(Op::ip(t, format!("route del {}/32", r.dst.v4)));
                routes.push(Op::ip(t, format!("route del {}/128", r.dst.v6)));
            }
        }
        routes.extend(default_ops(
            t,
            &net.default_via,
            &prev.and_then(|p| p.default_via.clone()),
        ));
    }
    deletes
        .into_iter()
        .chain(creates)
        .chain(config)
        .chain(routes)
        .collect()
}

pub fn initial_ops(desired: &Desired) -> Vec<Op> {
    diff(&Desired::new(), desired)
}

/// The operations that produce `net` from an empty namespace (`tpo explain`).
pub fn explain_ops(t: &Target, net: &RoutedNet) -> Vec<Op> {
    let mut ops = setup_ops(t, &net.own);
    for port in net.ports.values() {
        if net.own.index < port.peer.index {
            ops.push(create_port(t, port, &net.own));
        }
        ops.extend(configure_port(t, port, &net.own));
    }
    for r in net.routes.values() {
        ops.extend(route_ops(t, r));
    }
    ops.extend(default_ops(t, &net.default_via, &None));
    ops
}

/// Rebuilds a node's ports (tc, neighbours) and routes from scratch, keeping its veths.
pub fn rebuild_ops(t: &Target, net: &RoutedNet) -> Vec<Op> {
    let mut ops = Vec::new();
    for port in net.ports.values() {
        let dev = format!("p{}", port.peer.index);
        ops.push(Op::tc(t, format!("qdisc del dev {dev} root")));
        ops.push(Op::tc(t, format!("qdisc del dev {dev} clsact")));
        ops.push(Op::ip(t, format!("neigh flush dev {dev}")));
        ops.extend(neighbour_ops(t, &dev, &port.peer, port.peer_mac));
        ops.push(Op::tc(
            t,
            format!(
                "qdisc replace dev {dev} root netem {}",
                netem_args(&port.egress).join(" ")
            ),
        ));
        ops.push(Op::tc(t, format!("qdisc add dev {dev} clsact")));
        if !port.accept {
            ops.push(drop_op(t, &dev));
        }
    }
    for r in net.routes.values() {
        ops.extend(route_ops(t, r));
    }
    ops.extend(default_ops(t, &net.default_via, &None));
    ops
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::addr::allocate;
    use crate::model::{Edge, Topology};
    use crate::plan::{Faults, Until, effective};

    fn topo(src: &str) -> Topology {
        Topology::parse(&format!("mode = \"routed\"\n{src}"), "lab").unwrap()
    }

    fn plan(t: &Topology, f: &Faults, prev: Option<&Desired>) -> Desired {
        let addrs = allocate(t, None).unwrap();
        desired(&Ctx::new(t, &addrs), &effective(t, f, 0), prev)
    }

    fn rendered(ops: &[Op]) -> Vec<String> {
        ops.iter().map(ToString::to_string).collect()
    }

    const CHAIN: &str =
        "[nodes]\na={}\nb={}\nc={}\n[[link]]\npath=[\"a\",\"b\",\"c\"]\ndelay=\"10ms\"\n";
    const TRIANGLE: &str = "[nodes]\na={}\nb={}\nc={}\n[[link]]\npath=[\"a\",\"b\",\"c\"]\ndelay=\"10ms\"\n[[link]]\nfrom=\"a\"\nto=\"c\"\ndelay=\"50ms\"\n";

    fn next_hop(d: &Desired, from: &str, to_index: u32) -> Option<u32> {
        d[&Target::node(from)].routes.get(&to_index).map(|r| r.via)
    }

    #[test]
    fn routes_follow_the_shortest_path() {
        let d = plan(&topo(CHAIN), &Faults::default(), None);
        assert_eq!(next_hop(&d, "a", 2), Some(2));
        assert_eq!(next_hop(&d, "a", 3), Some(2));
        assert_eq!(next_hop(&d, "c", 1), Some(2));
    }

    #[test]
    fn route_ops_use_onlink_v4_and_link_local_v6_next_hops() {
        let t = topo(CHAIN);
        let d = plan(&t, &Faults::default(), None);
        let ops = rendered(&initial_ops(&d));
        for expected in [
            "a: ip addr add 10.77.0.1/32 dev lo",
            "a: ip addr add fd77::1/128 dev lo",
            "a: ip route replace 10.77.0.2/32 dev p2",
            "a: ip route replace fd77::2/128 dev p2",
            "a: ip route replace 10.77.0.3/32 via 10.77.0.2 dev p2 onlink",
            "a: ip route replace fd77::3/128 via fe80::2 dev p2",
            "a: ip addr add fe80::1/64 dev p2 nodad",
            "a: tc qdisc replace dev p2 root netem limit 10000 delay 10000us",
            "b: sysctl net.ipv4.ip_forward=1",
            "b: sysctl net.ipv6.conf.all.forwarding=1",
        ] {
            assert!(
                ops.contains(&expected.to_owned()),
                "missing `{expected}` in:\n{}",
                ops.join("\n")
            );
        }
    }

    #[test]
    fn each_veth_pair_is_created_once_by_the_lower_index() {
        let t = topo(CHAIN);
        let ops = rendered(&initial_ops(&plan(&t, &Faults::default(), None)));
        let creates: Vec<_> = ops.iter().filter(|o| o.contains("type veth")).collect();
        assert_eq!(creates.len(), 2, "{creates:?}");
        assert!(
            creates[0].starts_with("a: ip link add p2 address"),
            "{creates:?}"
        );
        assert!(
            creates[0].contains("peer name p1 address") && creates[0].ends_with("netns @netns:b"),
            "{creates:?}"
        );
        assert!(creates[1].starts_with("b: ip link add p3 "), "{creates:?}");
    }

    #[test]
    fn cutting_a_link_drops_at_the_receiver_and_reroutes() {
        let t = topo(TRIANGLE);
        let before = plan(&t, &Faults::default(), None);
        assert_eq!(next_hop(&before, "a", 3), Some(3), "hops metric: direct");
        let mut f = Faults::default();
        f.cut(&[Edge::new("a", "c"), Edge::new("c", "a")], Until::FOREVER);
        let after = plan(&t, &f, Some(&before));
        assert_eq!(next_hop(&after, "a", 3), Some(2), "fails over via b");
        let ops = rendered(&diff(&before, &after));
        assert!(
            ops.contains(
                &"c: tc filter add dev p1 ingress pref 1 protocol all matchall action drop"
                    .to_owned()
            ),
            "{ops:?}"
        );
        assert!(
            ops.contains(
                &"a: tc filter add dev p3 ingress pref 1 protocol all matchall action drop"
                    .to_owned()
            ),
            "{ops:?}"
        );
        assert!(
            ops.contains(
                &"a: ip route replace 10.77.0.3/32 via 10.77.0.2 dev p2 onlink".to_owned()
            ),
            "{ops:?}"
        );
    }

    #[test]
    fn without_any_path_the_previous_route_is_kept() {
        let t = topo(CHAIN);
        let before = plan(&t, &Faults::default(), None);
        let mut f = Faults::default();
        f.cut(&[Edge::new("b", "c"), Edge::new("c", "b")], Until::FOREVER);
        let after = plan(&t, &f, Some(&before));
        assert_eq!(
            next_hop(&after, "a", 3),
            Some(2),
            "packets still go towards the cut"
        );
        let ops = rendered(&diff(&before, &after));
        assert!(!ops.iter().any(|o| o.contains("route")), "{ops:?}");
    }

    #[test]
    fn a_missing_direction_is_dropped_at_the_receiver() {
        let t = topo("[nodes]\na={}\nb={}\n[[link]]\nfrom=\"a\"\nto=\"b\"\noneway=true\n");
        let ops = rendered(&initial_ops(&plan(&t, &Faults::default(), None)));
        assert!(
            ops.contains(
                &"a: tc filter add dev p2 ingress pref 1 protocol all matchall action drop"
                    .to_owned()
            ),
            "{ops:?}"
        );
        assert!(
            !ops.contains(
                &"b: tc filter add dev p1 ingress pref 1 protocol all matchall action drop"
                    .to_owned()
            ),
            "{ops:?}"
        );
    }

    #[test]
    fn the_delay_metric_prefers_faster_paths() {
        let t = Topology::parse(
            &format!("mode = \"routed\"\nmetric = \"delay\"\n{TRIANGLE}"),
            "lab",
        )
        .unwrap();
        let d = plan(&t, &Faults::default(), None);
        assert_eq!(next_hop(&d, "a", 3), Some(2), "10ms+10ms beats 50ms");
    }
}
