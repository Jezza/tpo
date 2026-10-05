//! The internet uplink (spec §6): pasta into the gateway netns, plus nft NAT and enforcement.

use super::Lab;
use crate::plan::{Op, Target};
use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::process::Command;

pub const DNS4: &str = "192.0.2.53";
pub const DNS6: &str = "2001:db8::53";
pub const HOST4: &str = "192.0.2.2";
const UPLINK: &str = "tap0";

pub type Allowed = BTreeSet<(Ipv4Addr, Ipv6Addr)>;

/// Which address families the host has a default route for.
pub fn host_families() -> (bool, bool) {
    let v4 = std::fs::read_to_string("/proc/net/route").is_ok_and(|t| {
        t.lines()
            .skip(1)
            .any(|l| l.split_whitespace().nth(1) == Some("00000000"))
    });
    let v6 = std::fs::read_to_string("/proc/net/ipv6_route").is_ok_and(|t| {
        t.lines().any(|l| {
            let f: Vec<_> = l.split_whitespace().collect();
            f.len() >= 10 && f[0].chars().all(|c| c == '0') && f[1] == "00" && f[9] != "lo"
        })
    });
    (v4, v6)
}

pub fn resolv_conf() -> String {
    let (v4, v6) = host_families();
    let mut out = String::new();
    if v4 {
        let _ = writeln!(out, "nameserver {DNS4}");
    }
    if v6 {
        let _ = writeln!(out, "nameserver {DNS6}");
    }
    out
}

fn elements(allowed: &Allowed) -> (String, String) {
    let v4: Vec<String> = allowed.iter().map(|(a, _)| a.to_string()).collect();
    let v6: Vec<String> = allowed.iter().map(|(_, a)| a.to_string()).collect();
    (v4.join(", "), v6.join(", "))
}

/// The gateway's nft ruleset. Membership in `allowed4`/`allowed6` is checked before anything
/// else, so removing a node also stops its open connections.
pub fn ruleset(allowed: &Allowed) -> String {
    gateway_ruleset(allowed, UPLINK, true)
}

/// The gateway's ruleset for an uplink interface; with `--kernel-nat` the host does the NAT.
pub fn gateway_ruleset(allowed: &Allowed, uplink: &str, masquerade: bool) -> String {
    let (v4, v6) = elements(allowed);
    let set = |name: &str, ty: &str, elems: &str| {
        if elems.is_empty() {
            format!("  set {name} {{ type {ty}; }}\n")
        } else {
            format!("  set {name} {{ type {ty}; elements = {{ {elems} }} }}\n")
        }
    };
    format!(
        "table inet tpo {{\n{}{}  chain forward {{\n    type filter hook forward priority 0; policy drop;\n    \
         oifname \"{uplink}\" ip saddr @allowed4 accept\n    oifname \"{uplink}\" ip6 saddr @allowed6 accept\n    \
         iifname \"{uplink}\" ip daddr @allowed4 accept\n    iifname \"{uplink}\" ip6 daddr @allowed6 accept\n  }}\n  \
         chain input {{\n    type filter hook input priority 0; policy drop;\n    iifname \"{uplink}\" accept\n    \
         iifname \"lo\" accept\n    ip saddr @allowed4 accept\n    ip6 saddr @allowed6 accept\n  }}\n  \
         {}}}\n",
        set("allowed4", "ipv4_addr", &v4),
        set("allowed6", "ipv6_addr", &v6),
        if masquerade {
            format!(
                "chain postrouting {{\n    type nat hook postrouting priority 100;\n    oifname \"{uplink}\" masquerade\n  }}\n"
            )
        } else {
            String::new()
        },
    )
}

/// Set updates from `old` to `new` (none when equal).
pub fn update_ops(old: &Allowed, new: &Allowed) -> Vec<Op> {
    let added: Allowed = new.difference(old).cloned().collect();
    let removed: Allowed = old.difference(new).cloned().collect();
    let mut script = String::new();
    for (verb, set) in [("delete", &removed), ("add", &added)] {
        if set.is_empty() {
            continue;
        }
        let (v4, v6) = elements(set);
        let _ = writeln!(script, "{verb} element inet tpo allowed4 {{ {v4} }}");
        let _ = writeln!(script, "{verb} element inet tpo allowed6 {{ {v6} }}");
    }
    if script.is_empty() {
        vec![]
    } else {
        vec![Op::nft(&Target::Gateway, script)]
    }
}

/// Starts pasta attached to the gateway netns; returns its pid once `tap0` is configured.
pub fn start_pasta(lab: &Lab, gateway_holder: i32) -> Result<i32, String> {
    let pasta = lab
        .tools
        .pasta
        .as_ref()
        .ok_or("internet access needs `pasta` (install the `passt` package)")?;
    let pidfile = lab.dir.path.join("pasta.pid");
    let mut cmd = Command::new(pasta);
    cmd.args(["--config-net", "-I", UPLINK]);
    if lab.init.user.is_some() {
        cmd.arg("--userns")
            .arg(format!("/proc/{}/ns/user", lab.init.pid));
    } else {
        // Root mode: switching to `nobody` would lose the capability to enter a netns owned by
        // the host's user namespace (verified), so pasta stays uid 0 inside its own sandbox.
        cmd.args(["--runas", "0"]);
    }
    // As root, pasta drops privileges before opening the netns and can no longer open another
    // process's /proc/<pid>/ns/net; root labs have the gateway's netns in /run/netns instead.
    let netns = match lab
        .nodes
        .get(&Target::Gateway)
        .and_then(|g| g.netns_name.as_deref())
    {
        Some(name) => format!("/run/netns/{name}"),
        None => format!("/proc/{gateway_holder}/ns/net"),
    };
    cmd.arg("--netns")
        .arg(netns)
        .args(["--dns-forward", DNS4, "--dns-forward", DNS6])
        .arg("--map-host-loopback")
        .arg(if lab.topo.internet.host_loopback {
            HOST4
        } else {
            "none"
        })
        .arg("-P")
        .arg(&pidfile);
    let out = cmd.output().map_err(|e| format!("can't run pasta: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "pasta failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    std::fs::read_to_string(&pidfile)
        .ok()
        .and_then(|p| p.trim().parse().ok())
        .ok_or_else(|| "pasta didn't write its pid".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allowed(n: u8) -> Allowed {
        [(
            Ipv4Addr::new(10, 77, 0, n),
            format!("fd77::{n}").parse().unwrap(),
        )]
        .into()
    }

    #[test]
    fn ruleset_checks_membership_before_anything_else() {
        let r = ruleset(&allowed(1));
        assert!(
            r.contains("elements = { 10.77.0.1 }") && r.contains("elements = { fd77::1 }"),
            "{r}"
        );
        assert!(
            !r.contains("established"),
            "no conntrack shortcut past the membership check"
        );
        assert!(r.contains("masquerade"));
        let empty = ruleset(&Allowed::new());
        assert!(
            empty.contains("set allowed4 { type ipv4_addr; }"),
            "{empty}"
        );
    }

    #[test]
    fn kernel_nat_gateways_filter_but_do_not_masquerade() {
        let r = gateway_ruleset(&allowed(1), "up0", false);
        assert!(
            r.contains("oifname \"up0\" ip saddr @allowed4 accept"),
            "{r}"
        );
        assert!(!r.contains("masquerade"), "{r}");
    }

    #[test]
    fn updates_add_and_delete_elements() {
        let ops = update_ops(&allowed(1), &allowed(2));
        let text = ops[0].to_string();
        assert!(
            text.contains("delete element inet tpo allowed4 { 10.77.0.1 }"),
            "{text}"
        );
        assert!(
            text.contains("add element inet tpo allowed6 { fd77::2 }"),
            "{text}"
        );
        assert!(update_ops(&allowed(1), &allowed(1)).is_empty());
    }
}
