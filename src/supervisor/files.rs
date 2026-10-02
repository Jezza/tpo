//! The lab's `/etc/hosts`, `/etc/resolv.conf` and `/etc/nsswitch.conf` (spec §5).

use crate::addr::Addresses;
use crate::labdir::LabDir;
use crate::model::{INTERNET, Topology};
use std::fmt::Write as _;
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::Path;

pub fn hosts(topo: &Topology, addrs: &Addresses) -> String {
    let mut out = String::from("127.0.0.1\tlocalhost\n::1\tlocalhost ip6-localhost ip6-loopback\n");
    for (node, a) in &addrs.nodes {
        let _ = writeln!(out, "{}\t{node}", a.v4);
        let _ = writeln!(out, "{}\t{node}", a.v6);
    }
    if topo.uses_internet() {
        let _ = writeln!(out, "{}\t{INTERNET}", addrs.gateway.v4);
        let _ = writeln!(out, "{}\t{INTERNET}", addrs.gateway.v6);
        if topo.internet.host_loopback {
            let _ = writeln!(out, "{}\thost", super::uplink::HOST4);
        }
    }
    out
}

/// Lookups stay inside the node: files, then DNS (spec §5).
pub fn nsswitch(host: &str) -> String {
    let mut out = String::from("hosts: files dns\n");
    for line in host.lines() {
        if !line.trim_start().starts_with("hosts:") {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// Rewrites a bind-mounted file in place (truncate + write), so the mount keeps pointing at it.
fn write_in_place(path: &Path, contents: &str) -> io::Result<()> {
    let mut f = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)?;
    f.write_all(contents.as_bytes())
}

pub fn write_etc(dir: &LabDir, topo: &Topology, addrs: &Addresses) -> io::Result<()> {
    let etc = dir.etc();
    write_in_place(&etc.join("hosts"), &hosts(topo, addrs))?;
    let resolv = if topo.uses_internet() {
        super::uplink::resolv_conf()
    } else {
        String::new()
    };
    write_in_place(&etc.join("resolv.conf"), &resolv)?;
    let host = std::fs::read_to_string("/etc/nsswitch.conf").unwrap_or_default();
    write_in_place(&etc.join("nsswitch.conf"), &nsswitch(&host))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::addr::allocate;

    #[test]
    fn hosts_lists_every_node_on_both_families() {
        let t = Topology::parse("[nodes]\na={}\nb={}\n", "lab").unwrap();
        let h = hosts(&t, &allocate(&t, None).unwrap());
        assert!(
            h.contains("10.77.0.2\tb\n") && h.contains("fd77::2\tb\n"),
            "{h}"
        );
        assert!(!h.contains(INTERNET));
    }

    #[test]
    fn nsswitch_keeps_other_databases_but_resolves_hosts_locally() {
        let n = nsswitch(
            "passwd: files systemd\nhosts: files mdns4_minimal [NOTFOUND=return] dns myhostname\n",
        );
        assert_eq!(n, "hosts: files dns\npasswd: files systemd\n");
    }
}
