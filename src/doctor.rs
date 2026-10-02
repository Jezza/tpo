//! `tpo doctor` (spec §8): preflight checks with a cause and a fix for each failure.

use crate::backend::{self, NetFds, Tools};
use crate::enter::{self, EnterSpec, NodeSetup};
use crate::labdir::tool;
use crate::ns;
use crate::plan::{Op, Target};
use std::collections::HashMap;
use std::os::fd::AsRawFd;
use std::process::Stdio;

pub type Sysctls = HashMap<String, String>;

const USERNS_SYSCTLS: [&str; 3] = [
    "kernel.apparmor_restrict_unprivileged_userns",
    "kernel.unprivileged_userns_clone",
    "user.max_user_namespaces",
];

pub fn read_sysctls() -> Sysctls {
    USERNS_SYSCTLS
        .iter()
        .filter_map(|k| {
            let v = std::fs::read_to_string(format!("/proc/sys/{}", k.replace('.', "/"))).ok()?;
            Some(((*k).to_owned(), v.trim().to_owned()))
        })
        .collect()
}

/// Why unprivileged user namespaces don't work here, if a known setting explains it.
pub fn userns_hint(sysctls: &Sysctls) -> Option<String> {
    let is = |k: &str, v: &str| sysctls.get(k).is_some_and(|x| x == v);
    if is("kernel.apparmor_restrict_unprivileged_userns", "1") {
        return Some(
            "AppArmor restricts unprivileged user namespaces (Ubuntu ≥ 23.10). Run with `--root`, \
             or allow them: `sudo sysctl kernel.apparmor_restrict_unprivileged_userns=0`"
                .to_owned(),
        );
    }
    if is("kernel.unprivileged_userns_clone", "0") {
        return Some(
            "unprivileged user namespaces are disabled. Run with `--root`, or enable them: \
             `sudo sysctl kernel.unprivileged_userns_clone=1`"
                .to_owned(),
        );
    }
    if is("user.max_user_namespaces", "0") {
        return Some(
            "user namespaces are disabled (`user.max_user_namespaces=0`). Run with `--root`, or \
             raise it: `sudo sysctl user.max_user_namespaces=10000`"
                .to_owned(),
        );
    }
    None
}

/// Containers usually block user namespaces with seccomp.
pub fn container_hint(cgroup: &str, dockerenv: bool) -> Option<String> {
    let inside = dockerenv
        || ["docker", "containerd", "kubepods", "libpod", "lxc"]
            .iter()
            .any(|m| cgroup.contains(m));
    inside.then(|| {
        "this looks like a container, whose seccomp profile usually blocks user namespaces. Run on the \
         host, use a privileged container, or `--root`"
            .to_owned()
    })
}

/// Why a `tc` command failed, if it's a known kernel limitation.
pub fn tc_hint(stderr: &str) -> Option<String> {
    (stderr.contains("qdisc kind is unknown")
        || stderr.contains("Unknown filter")
        || stderr.contains("Unknown action"))
    .then(|| {
        "the kernel lacks a tc module (or can't load it from a user namespace). Load them once: \
             `sudo modprobe sch_netem sch_htb sch_ingress cls_flower cls_matchall act_gact`"
            .to_owned()
    })
}

/// A hint for an error from bringing a lab up, if one applies.
pub fn hint_for(error: &str) -> Option<String> {
    if let Some(h) = tc_hint(error) {
        return Some(h);
    }
    let denied = [
        "Operation not permitted",
        "EPERM",
        "Permission denied",
        "EACCES",
    ]
    .iter()
    .any(|m| error.contains(m));
    if !denied {
        return None;
    }
    userns_hint(&read_sysctls()).or_else(|| {
        let cgroup = std::fs::read_to_string("/proc/self/cgroup").unwrap_or_default();
        container_hint(&cgroup, std::path::Path::new("/.dockerenv").exists())
    })
}

pub struct Check {
    pub name: String,
    pub result: Result<String, String>,
    /// Labs can't run without it (otherwise it's only needed for `internet`).
    pub required: bool,
}

impl Check {
    fn new(name: &str, required: bool, result: Result<String, String>) -> Self {
        Check {
            name: name.to_owned(),
            result,
            required,
        }
    }
}

/// Runs every check. Must be called while the process is single-threaded.
pub fn run() -> Vec<Check> {
    let mut checks = Vec::new();
    let found = |name: &str| tool(name).map(|p| p.display().to_string());
    checks.push(Check::new(
        "iproute2 (`ip`, `tc`)",
        true,
        match (found("ip"), found("tc")) {
            (Some(ip), Some(tc)) => Ok(format!("{ip}, {tc}")),
            _ => Err("not found; install iproute2".to_owned()),
        },
    ));
    checks.push(Check::new(
        "nftables (`nft`, for internet)",
        false,
        found("nft").ok_or_else(|| "not found; install nftables".to_owned()),
    ));
    checks.push(Check::new(
        "pasta (for internet)",
        false,
        found("pasta").ok_or_else(|| "not found; install passt".to_owned()),
    ));
    checks.push(Check::new(
        "/dev/net/tun (for internet)",
        false,
        if std::path::Path::new("/dev/net/tun").exists() {
            Ok("present".to_owned())
        } else {
            Err("missing".to_owned())
        },
    ));
    let (v4, v6) = crate::supervisor::host_families();
    checks.push(Check::new(
        "host connectivity (for internet)",
        false,
        match (v4, v6) {
            (false, false) => Err("no default route on the host".to_owned()),
            _ => Ok(format!("IPv4 {}, IPv6 {}", yes(v4), yes(v6))),
        },
    ));

    let Ok(tools) = Tools::find() else {
        return checks;
    };
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(e) => {
            checks.push(Check::new("tpo executable", true, Err(e.to_string())));
            return checks;
        }
    };
    let init = match ns::spawn_init(&exe, true) {
        Ok(init) => init,
        Err(e) => {
            let msg = e.to_string();
            checks.push(Check::new(
                "user + network + pid namespaces",
                true,
                Err(with_hint(&msg)),
            ));
            return checks;
        }
    };
    checks.push(Check::new(
        "user + network + pid namespaces",
        true,
        Ok("created".to_owned()),
    ));

    let fds = || NetFds {
        user: init.user.try_clone().ok(),
        hub_pid: init.pid.as_raw(),
        nets: [(Target::Hub, init.net.try_clone().expect("dup netns fd"))].into(),
    };
    let hub = Target::Hub;
    let probes: [(&str, bool, Vec<Op>); 4] = [
        (
            "bridge and veth",
            true,
            vec![
                Op::ip(&hub, "link add tpo-br type bridge"),
                Op::ip(&hub, "link add tpo-v0 type veth peer name tpo-v1"),
                Op::ip(&hub, "link set tpo-v0 master tpo-br"),
            ],
        ),
        (
            "tc: HTB + netem",
            true,
            vec![
                Op::tc(&hub, "qdisc add dev tpo-v1 root handle 1: htb default fffe"),
                Op::tc(
                    &hub,
                    "class add dev tpo-v1 parent 1: classid 1:2 htb rate 10gbit ceil 10gbit quantum 200000",
                ),
                Op::tc(&hub, "qdisc add dev tpo-v1 parent 1:2 netem delay 1ms"),
            ],
        ),
        (
            "tc: flower, clsact, matchall",
            true,
            vec![
                Op::tc(
                    &hub,
                    "filter add dev tpo-v1 parent 1: pref 2 protocol all flower dst_mac 02:74:00:00:00:02 classid 1:2",
                ),
                Op::tc(&hub, "qdisc add dev tpo-v1 clsact"),
                Op::tc(
                    &hub,
                    "filter add dev tpo-v1 ingress pref 1 protocol all matchall action drop",
                ),
            ],
        ),
        (
            "nftables NAT (for internet)",
            false,
            vec![Op::nft(
                &hub,
                "table inet tpo-probe {\n  chain post {\n    type nat hook postrouting priority 100;\n    masquerade\n  }\n}\n",
            )],
        ),
    ];
    for (name, required, ops) in probes {
        if !required && tools.nft.is_none() {
            continue;
        }
        let result = backend::run(&fds(), &tools, &ops)
            .map(|()| "works".to_owned())
            .map_err(|e| with_hint(&e));
        checks.push(Check::new(name, required, result));
    }

    let spec = EnterSpec {
        user: Some(init.user.as_raw_fd()),
        pid: Some(init.pid_ns.as_raw_fd()),
        setup: Some(NodeSetup {
            hostname: "tpo-doctor".to_owned(),
            binds: vec![],
        }),
        hold: true,
        ..EnterSpec::default()
    };
    let result = match enter::spawn(&exe, spec, Stdio::piped(), Stdio::null(), Stdio::piped()) {
        Ok((mut child, _)) => {
            drop(child.stdin.take());
            let _ = child.wait();
            Ok("works".to_owned())
        }
        Err(e) => Err(with_hint(&e)),
    };
    checks.push(Check::new("mount, UTS namespaces and /proc", true, result));
    let _ = nix::sys::signal::kill(init.pid, nix::sys::signal::Signal::SIGKILL);
    checks
}

fn yes(b: bool) -> &'static str {
    if b { "yes" } else { "no" }
}

fn with_hint(error: &str) -> String {
    match hint_for(error) {
        Some(h) => format!("{error}\n    fix: {h}"),
        None => error.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sysctls(pairs: &[(&str, &str)]) -> Sysctls {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn explains_apparmor_userns_restrictions() {
        let hint = userns_hint(&sysctls(&[(
            "kernel.apparmor_restrict_unprivileged_userns",
            "1",
        )]))
        .unwrap();
        assert!(
            hint.contains("AppArmor") && hint.contains("--root") && hint.contains("=0"),
            "{hint}"
        );
    }

    #[test]
    fn explains_disabled_user_namespaces() {
        let hint = userns_hint(&sysctls(&[("kernel.unprivileged_userns_clone", "0")])).unwrap();
        assert!(hint.contains("unprivileged_userns_clone=1"), "{hint}");
        let hint = userns_hint(&sysctls(&[("user.max_user_namespaces", "0")])).unwrap();
        assert!(hint.contains("max_user_namespaces"), "{hint}");
    }

    #[test]
    fn no_hint_when_nothing_restricts_user_namespaces() {
        let ok = sysctls(&[
            ("kernel.apparmor_restrict_unprivileged_userns", "0"),
            ("kernel.unprivileged_userns_clone", "1"),
            ("user.max_user_namespaces", "1000"),
        ]);
        assert_eq!(userns_hint(&ok), None);
    }

    #[test]
    fn recognises_containers() {
        assert!(
            container_hint("0::/docker/abc\n", false)
                .unwrap()
                .contains("container")
        );
        assert!(container_hint("", true).unwrap().contains("container"));
        assert_eq!(
            container_hint("0::/user.slice/user-1000.slice\n", false),
            None
        );
    }

    #[test]
    fn explains_missing_qdisc_modules() {
        let hint = tc_hint("Error: Specified qdisc kind is unknown.").unwrap();
        assert!(hint.contains("modprobe sch_netem"), "{hint}");
        assert_eq!(tc_hint("RTNETLINK answers: File exists"), None);
    }
}
