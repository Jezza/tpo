//! `--kernel-nat` (spec §6): a veth from the gateway into the host netns, with the host doing
//! NAT in the kernel. Root only.

use crate::addr::{Addresses, Mac, mac};
use crate::model::Topology;
use crate::plan::{Op, Target};
use std::fmt::Write as _;
use std::net::{Ipv4Addr, Ipv6Addr};

/// The gateway's end of the uplink veth.
pub const GW_IF: &str = "up0";
/// The host's (address-less) next hop, as the gateway sees it.
const HOST_HOP4: &str = "169.254.0.1";

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct KernelNat {
    pub host_if: String,
    pub table: String,
    pub host_mac: Mac,
    pub gw_mac: Mac,
}

/// The first nameserver of each family in a `resolv.conf`.
pub fn resolvers(resolv_conf: &str) -> (Option<Ipv4Addr>, Option<Ipv6Addr>) {
    let mut v4 = None;
    let mut v6 = None;
    for line in resolv_conf.lines() {
        let mut words = line.split_whitespace();
        if words.next() != Some("nameserver") {
            continue;
        }
        match words.next().map(str::parse::<std::net::IpAddr>) {
            Some(Ok(std::net::IpAddr::V4(a))) if v4.is_none() => v4 = Some(a),
            Some(Ok(std::net::IpAddr::V6(a))) if v6.is_none() => v6 = Some(a),
            _ => {}
        }
    }
    (v4, v6)
}

impl KernelNat {
    pub fn new(hash: u16) -> Self {
        KernelNat {
            host_if: format!("tpo{hash:04x}up"),
            table: format!("tpo_{hash:04x}"),
            // Indices 0xfff0/0xfff1 are above any node index (subnets are /17 or longer).
            host_mac: mac(hash, 0xfff0),
            gw_mac: mac(hash, 0xfff1),
        }
    }

    /// IPv6 has no `route_localnet`, so a loopback IPv6 resolver can't be reached via DNAT.
    pub fn forwards_dns6(&self, dns6: Option<Ipv6Addr>) -> bool {
        dns6.is_some_and(|a| !a.is_loopback())
    }

    pub fn host_ruleset(
        &self,
        topo: &Topology,
        dns4: Option<Ipv4Addr>,
        dns6: Option<Ipv6Addr>,
    ) -> String {
        let up = &self.host_if;
        let mut pre = String::new();
        if let Some(d) = dns4 {
            for proto in ["udp", "tcp"] {
                let _ = writeln!(
                    pre,
                    "    iifname \"{up}\" ip daddr {} {proto} dport 53 dnat ip to {d}",
                    super::uplink::DNS4
                );
            }
        }
        if let Some(d) = dns6.filter(|_| self.forwards_dns6(dns6)) {
            for proto in ["udp", "tcp"] {
                let _ = writeln!(
                    pre,
                    "    iifname \"{up}\" ip6 daddr {} {proto} dport 53 dnat ip6 to {d}",
                    super::uplink::DNS6
                );
            }
        }
        if topo.internet.host_loopback {
            let _ = writeln!(
                pre,
                "    iifname \"{up}\" ip daddr {} dnat ip to 127.0.0.1",
                super::uplink::HOST4
            );
        }
        format!(
            "table inet {table} {{\n  chain prerouting {{\n    type nat hook prerouting priority -100;\n{pre}  }}\n  \
             chain postrouting {{\n    type nat hook postrouting priority 100;\n    \
             ip saddr {v4} oifname != \"{up}\" masquerade\n    ip6 saddr {v6} oifname != \"{up}\" masquerade\n  }}\n  \
             chain forward {{\n    type filter hook forward priority 0;\n    iifname \"{up}\" accept\n    \
             oifname \"{up}\" accept\n  }}\n}}\n",
            table = self.table,
            v4 = topo.subnet,
            v6 = topo.subnet6,
        )
    }

    /// The veth, routes and static neighbours between the gateway and the host.
    pub fn setup_ops(&self, topo: &Topology, addrs: &Addresses, route_localnet: bool) -> Vec<Op> {
        let (host, gw) = (Target::Host, Target::Gateway);
        let up = &self.host_if;
        let g = addrs.gateway;
        let mut ops = vec![
            Op::ip(
                &host,
                format!(
                    "link add {up} address {} type veth peer name {GW_IF} address {} netns @netns:gateway",
                    self.host_mac, self.gw_mac
                ),
            ),
            Op::ip(&host, format!("link set {up} addrgenmode none")),
            Op::ip(&host, format!("addr add fe80::1/64 dev {up} nodad")),
            Op::ip(&host, format!("link set {up} up")),
            Op::ip(
                &host,
                format!(
                    "neigh replace {} lladdr {} dev {up} nud permanent",
                    g.v4, self.gw_mac
                ),
            ),
            Op::ip(
                &host,
                format!(
                    "neigh replace fe80::2 lladdr {} dev {up} nud permanent",
                    self.gw_mac
                ),
            ),
            Op::ip(
                &host,
                format!("route replace {} via {} dev {up} onlink", topo.subnet, g.v4),
            ),
            Op::ip(
                &host,
                format!("route replace {} via fe80::2 dev {up}", topo.subnet6),
            ),
        ];
        if route_localnet {
            ops.push(Op::sysctl(
                &host,
                &format!("net.ipv4.conf.{up}.route_localnet"),
                "1",
            ));
        }
        ops.extend([
            Op::ip(&gw, format!("link set {GW_IF} addrgenmode none")),
            Op::ip(&gw, format!("addr add fe80::2/64 dev {GW_IF} nodad")),
            Op::ip(&gw, format!("link set {GW_IF} up")),
            Op::ip(
                &gw,
                format!(
                    "neigh replace {HOST_HOP4} lladdr {} dev {GW_IF} nud permanent",
                    self.host_mac
                ),
            ),
            Op::ip(
                &gw,
                format!(
                    "neigh replace fe80::1 lladdr {} dev {GW_IF} nud permanent",
                    self.host_mac
                ),
            ),
            Op::ip(
                &gw,
                format!("route replace default via {HOST_HOP4} dev {GW_IF} onlink"),
            ),
            Op::ip(&gw, format!("route replace ::/0 via fe80::1 dev {GW_IF}")),
        ]);
        ops
    }

    /// Removes everything `setup_ops` and the host ruleset created on the host.
    pub fn teardown_ops(&self) -> Vec<Op> {
        vec![
            Op::ip(&Target::Host, format!("link del {}", self.host_if)),
            Op::nft(&Target::Host, format!("delete table inet {}", self.table)),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::addr::allocate;
    use crate::model::Topology;

    fn plan(src: &str) -> (Topology, crate::addr::Addresses) {
        let t = Topology::parse(src, "lab").unwrap();
        let a = allocate(&t, None).unwrap();
        (t, a)
    }

    const LAB: &str = "[nodes]\na={}\n[[link]]\nfrom=\"a\"\nto=\"internet\"\n";

    #[test]
    fn interface_names_fit_ifnamsiz() {
        let k = KernelNat::new(0xbeef);
        assert_eq!(k.host_if, "tpobeefup");
        assert!(k.host_if.len() <= 15);
        assert_eq!(k.table, "tpo_beef");
    }

    #[test]
    fn finds_the_first_resolver_of_each_family() {
        let r = resolvers(
            "# c\nsearch x\nnameserver 127.0.0.53\nnameserver 9.9.9.9\nnameserver ::1\nnameserver 2001:db8::5\n",
        );
        assert_eq!(
            r,
            (
                Some("127.0.0.53".parse().unwrap()),
                Some("::1".parse().unwrap())
            )
        );
        assert_eq!(resolvers(""), (None, None));
    }

    #[test]
    fn host_ruleset_masquerades_lab_traffic_and_forwards_dns() {
        let k = KernelNat::new(0xbeef);
        let (t, _) = plan(LAB);
        let r = k.host_ruleset(
            &t,
            Some("127.0.0.53".parse().unwrap()),
            Some("2001:db8::5".parse().unwrap()),
        );
        assert!(r.contains("table inet tpo_beef"), "{r}");
        assert!(
            r.contains("ip saddr 10.77.0.0/24 oifname != \"tpobeefup\" masquerade"),
            "{r}"
        );
        assert!(
            r.contains("ip6 saddr fd77::/64 oifname != \"tpobeefup\" masquerade"),
            "{r}"
        );
        assert!(
            r.contains("ip daddr 192.0.2.53 udp dport 53 dnat ip to 127.0.0.53"),
            "{r}"
        );
        assert!(
            r.contains("ip6 daddr 2001:db8::53 udp dport 53 dnat ip6 to 2001:db8::5"),
            "{r}"
        );
        assert!(
            !r.contains("192.0.2.2"),
            "no host loopback unless asked: {r}"
        );
    }

    #[test]
    fn loopback_ipv6_resolvers_are_not_forwarded() {
        let k = KernelNat::new(1);
        let (t, _) = plan(LAB);
        let r = k.host_ruleset(&t, None, Some("::1".parse().unwrap()));
        assert!(!r.contains("2001:db8::53"), "{r}");
        assert!(!k.forwards_dns6(Some("::1".parse().unwrap())));
    }

    #[test]
    fn host_loopback_is_dnated_to_127_0_0_1() {
        let k = KernelNat::new(1);
        let (t, _) = plan(&format!("{LAB}[internet]\nhost_loopback = true\n"));
        let r = k.host_ruleset(&t, None, None);
        assert!(r.contains("ip daddr 192.0.2.2 dnat ip to 127.0.0.1"), "{r}");
    }

    #[test]
    fn setup_links_the_gateway_to_the_host_without_addresses() {
        let k = KernelNat::new(0xbeef);
        let (t, a) = plan(LAB);
        let ops: Vec<String> = k
            .setup_ops(&t, &a, true)
            .iter()
            .map(ToString::to_string)
            .collect();
        for expected in [
            format!(
                "host: ip link add tpobeefup address {} type veth peer name up0 address {} netns @netns:gateway",
                k.host_mac, k.gw_mac
            ),
            format!(
                "host: ip neigh replace 10.77.0.254 lladdr {} dev tpobeefup nud permanent",
                k.gw_mac
            ),
            "host: ip route replace 10.77.0.0/24 via 10.77.0.254 dev tpobeefup onlink".to_owned(),
            "host: ip route replace fd77::/64 via fe80::2 dev tpobeefup".to_owned(),
            "host: sysctl net.ipv4.conf.tpobeefup.route_localnet=1".to_owned(),
            format!(
                "gateway: ip neigh replace 169.254.0.1 lladdr {} dev up0 nud permanent",
                k.host_mac
            ),
            "gateway: ip route replace default via 169.254.0.1 dev up0 onlink".to_owned(),
            "gateway: ip route replace ::/0 via fe80::1 dev up0".to_owned(),
        ] {
            assert!(
                ops.contains(&expected),
                "missing `{expected}` in:\n{}",
                ops.join("\n")
            );
        }
        let teardown: Vec<String> = k.teardown_ops().iter().map(ToString::to_string).collect();
        assert_eq!(
            teardown,
            [
                "host: ip link del tpobeefup",
                "host: nft delete table inet tpo_beef"
            ]
        );
    }
}
