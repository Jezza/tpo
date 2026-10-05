//! Root mode (spec §2, §6): run with `sudo`. Skipped otherwise.
//!
//! These tests change the host: they create `/run/netns` entries and, for `--kernel-nat`, a veth,
//! an nft table and forwarding sysctls — and check that every one of them is undone.

mod common;

use common::*;
use std::process::Command;
use std::time::Duration;

fn is_root() -> bool {
    let root = nix::unistd::geteuid().is_root();
    if !root {
        eprintln!("SKIPPED: root-mode tests need root (run the test binary with sudo)");
    }
    root
}

const LAB: &str = r#"
[nodes]
a = {}
b = {}

[[link]]
path = ["a", "b"]
delay = "10ms"

[[link]]
from = "a"
to = "internet"

[run]
"*" = "sleep 600"
"#;

fn netns_list() -> String {
    String::from_utf8_lossy(
        &Command::new("ip")
            .args(["netns", "list"])
            .output()
            .unwrap()
            .stdout,
    )
    .into_owned()
}

fn curl(lab: &Lab, node: &str, args: &[&str]) -> bool {
    let mut cmd = vec!["curl", "-s", "-m8", "-o/dev/null"];
    cmd.extend_from_slice(args);
    lab.exec(node, &cmd).status.success()
}

fn host_has_ipv6() -> bool {
    Command::new("curl")
        .args([
            "-6",
            "-s",
            "-m5",
            "-o/dev/null",
            "http://[2606:4700:4700::1111]/",
        ])
        .status()
        .is_ok_and(|s| s.success())
}

/// The forwarding-related sysctls `--kernel-nat` may touch.
fn forwarding_sysctls() -> Vec<(String, String)> {
    let mut keys = vec![
        "net/ipv4/ip_forward".to_owned(),
        "net/ipv6/conf/all/forwarding".to_owned(),
    ];
    for e in std::fs::read_dir("/proc/sys/net/ipv6/conf")
        .unwrap()
        .flatten()
    {
        keys.push(format!(
            "net/ipv6/conf/{}/accept_ra",
            e.file_name().to_string_lossy()
        ));
    }
    keys.sort();
    keys.into_iter()
        .filter_map(|k| {
            Some((
                k.clone(),
                std::fs::read_to_string(format!("/proc/sys/{k}"))
                    .ok()?
                    .trim()
                    .to_owned(),
            ))
        })
        .collect()
}

fn kernel_nat_names(lab: &str) -> (String, String) {
    let hash = tpo::addr::lab_hash(lab);
    (format!("tpo{hash:04x}up"), format!("tpo_{hash:04x}"))
}

fn link_exists(name: &str) -> bool {
    Command::new("ip")
        .args(["link", "show", name])
        .output()
        .unwrap()
        .status
        .success()
}

fn nft_table_exists(table: &str) -> bool {
    Command::new("nft")
        .args(["list", "table", "inet", table])
        .output()
        .unwrap()
        .status
        .success()
}

#[test]
fn root_labs_are_visible_to_ip_netns_and_cleaned_up() {
    if !is_root() {
        return;
    }
    let lab = Lab::new("rootns", LAB);
    lab.ok(&["up", "-d", "--root", lab.file().to_str().unwrap()]);
    assert!(netns_list().contains("tpo-rootns-a"), "{}", netns_list());
    let out = Command::new("ip")
        .args([
            "netns",
            "exec",
            "tpo-rootns-a",
            "ping",
            "-c1",
            "-W2",
            "10.77.0.2",
        ])
        .output()
        .unwrap();
    assert!(out.status.success(), "other tools can use the lab's netns");
    assert!(lab.exec("a", &["ping", "-c1", "-W2", "b"]).status.success());
    lab.ok(&["down"]);
    assert!(!netns_list().contains("tpo-rootns-"), "{}", netns_list());
    assert!(!lab.lab_dir().exists());
}

#[test]
fn root_labs_use_pasta_for_the_internet_by_default() {
    if !is_root() {
        return;
    }
    let lab = Lab::new("rootpasta", LAB);
    lab.ok(&["up", "-d", "--root", lab.file().to_str().unwrap()]);
    assert!(curl(&lab, "a", &["https://cloudflare.com/"]));
    assert!(
        !curl(&lab, "b", &["http://1.1.1.1/"]),
        "b has no internet link"
    );
    assert!(lab.ok(&["show"]).contains("internet: up (pasta pid "));
    let (veth, _) = kernel_nat_names("rootpasta");
    assert!(
        !link_exists(&veth),
        "pasta mode changes nothing on the host"
    );
}

#[test]
fn kernel_nat_reaches_the_internet_and_restores_the_host() {
    if !is_root() {
        return;
    }
    let before = forwarding_sysctls();
    let lab = Lab::new("rootknat", LAB);
    let (veth, table) = kernel_nat_names("rootknat");
    lab.ok(&[
        "up",
        "-d",
        "--root",
        "--kernel-nat",
        lab.file().to_str().unwrap(),
    ]);
    assert!(link_exists(&veth) && nft_table_exists(&table));
    assert!(curl(&lab, "a", &["http://1.1.1.1/"]), "IPv4 by IP");
    assert!(
        curl(&lab, "a", &["https://cloudflare.com/"]),
        "by name (DNS through the host)"
    );
    if host_has_ipv6() {
        assert!(
            curl(&lab, "a", &["-6", "http://[2606:4700:4700::1111]/"]),
            "IPv6 by IP"
        );
    }
    let ping = lab.exec("a", &["ping", "-c1", "-W3", "1.1.1.1"]);
    assert!(ping.status.success(), "kernel NAT carries ICMP");
    assert!(
        !curl(&lab, "b", &["http://1.1.1.1/"]),
        "b has no internet link"
    );
    lab.ok(&["cut", "a", "internet"]);
    assert!(
        !curl(&lab, "a", &["http://1.1.1.1/"]),
        "cut works with kernel NAT too"
    );
    lab.ok(&["down"]);
    assert!(
        !link_exists(&veth) && !nft_table_exists(&table),
        "host changes are removed"
    );
    assert_eq!(forwarding_sysctls(), before, "sysctls are restored");
}

#[test]
fn down_force_cleans_up_after_a_crashed_root_lab() {
    if !is_root() {
        return;
    }
    for (name, extra) in [("rootcrash", vec![]), ("rootcrashk", vec!["--kernel-nat"])] {
        let before = forwarding_sysctls();
        let lab = Lab::new(name, LAB);
        let mut args = vec!["up", "-d", "--root"];
        args.extend(&extra);
        let file = lab.file();
        args.push(file.to_str().unwrap());
        lab.ok(&args);
        let sup = lab.supervisor_pid();
        nix::sys::signal::kill(nix::unistd::Pid::from_raw(sup), nix::sys::signal::SIGKILL).unwrap();
        assert!(wait_until(Duration::from_secs(5), || !pid_alive(sup)));
        assert!(
            netns_list().contains(&format!("tpo-{name}-a")),
            "netns outlive the supervisor"
        );

        let again = lab.tpo(&args);
        assert!(!again.status.success());
        assert!(
            String::from_utf8_lossy(&again.stderr).contains("down --force"),
            "{again:?}"
        );

        let out = lab.ok(&["down", "--force", "--lab", name]);
        assert!(
            out.contains(&format!("deleted netns tpo-{name}-a")),
            "{out}"
        );
        assert!(
            !netns_list().contains(&format!("tpo-{name}-")),
            "{}",
            netns_list()
        );
        assert!(!lab.lab_dir().exists());
        let (veth, table) = kernel_nat_names(name);
        assert!(!link_exists(&veth) && !nft_table_exists(&table));
        assert_eq!(forwarding_sysctls(), before, "{name}: sysctls are restored");
    }
}
