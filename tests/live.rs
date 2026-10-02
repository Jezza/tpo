//! Live changes (spec §7.3–§7.5): cut, set, heal, timers, apply, process control.

mod common;

use common::*;
use std::process::{Command, Stdio};
use std::time::Duration;

const CHAIN: &str = r#"
[nodes]
a = {}
b = {}
c = {}

[[link]]
path = ["a", "b", "c"]
delay = "20ms"
"#;

fn pings(lab: &Lab, from: &str, to: &str) -> bool {
    lab.exec(from, &["ping", "-c1", "-W1", to]).status.success()
}

fn rtt(lab: &Lab, from: &str, to: &str) -> f64 {
    let out = lab.exec(from, &["ping", "-c1", "-W2", to]);
    ping_rtt_ms(&out).unwrap_or_else(|| {
        panic!(
            "{from}→{to} ping failed: {}",
            String::from_utf8_lossy(&out.stdout)
        )
    })
}

/// Sends one UDP datagram `from` → `to` and reports whether it arrived.
fn udp_delivers(lab: &Lab, from: &str, to: &str, v6: bool) -> bool {
    let family = if v6 { "AF_INET6" } else { "AF_INET" };
    let any = if v6 { "::" } else { "0.0.0.0" };
    let recv = format!(
        "import socket; s=socket.socket(socket.{family}, socket.SOCK_DGRAM); s.bind(('{any}', 9999)); \
         s.settimeout(3); print(s.recvfrom(100)[0].decode())"
    );
    let receiver = Command::new(env!("CARGO_BIN_EXE_tpo"))
        .args(["exec", to, "--", "python3", "-c", &recv])
        .env("XDG_RUNTIME_DIR", lab.runtime_dir())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(400));
    let addr_var = format!(
        "TPO_ADDR{}_{}",
        if v6 { "6" } else { "" },
        to.to_uppercase()
    );
    let send = format!(
        "import os, socket; s=socket.socket(socket.{family}, socket.SOCK_DGRAM); \
         s.sendto(b'hi', (os.environ['{addr_var}'], 9999))"
    );
    lab.exec(from, &["python3", "-c", &send]);
    let out = receiver.wait_with_output().unwrap();
    String::from_utf8_lossy(&out.stdout).contains("hi")
}

#[test]
fn cut_and_heal() {
    if !userns_available() {
        return;
    }
    let lab = Lab::new("cutheal", CHAIN);
    lab.up();
    assert!(pings(&lab, "a", "b"));
    let out = lab.ok(&["cut", "a", "b"]);
    assert!(out.contains("cut a⇄b"), "{out}");
    assert!(!pings(&lab, "a", "b"));
    assert!(pings(&lab, "b", "c"), "other links are untouched");
    lab.ok(&["heal", "a", "b"]);
    assert!(pings(&lab, "a", "b"));
}

#[test]
fn oneway_cuts_stay_one_way_for_ipv4_and_ipv6() {
    if !userns_available() {
        return;
    }
    let lab = Lab::new("oneway", CHAIN);
    lab.up();
    lab.ok(&["cut", "--oneway", "b", "a"]);
    for v6 in [false, true] {
        assert!(
            udp_delivers(&lab, "a", "b", v6),
            "a→b still delivers (v6={v6})"
        );
        assert!(!udp_delivers(&lab, "b", "a", v6), "b→a is cut (v6={v6})");
    }
}

#[test]
fn set_changes_delay_and_dropping_the_override_restores_it() {
    if !userns_available() {
        return;
    }
    let lab = Lab::new("setdelay", CHAIN);
    lab.up();
    let out = lab.ok(&["set", "a", "b", "delay=100ms"]);
    assert!(out.contains("delay 20ms→100ms"), "{out}");
    let r = rtt(&lab, "a", "b");
    assert!(
        (195.0..260.0).contains(&r),
        "100ms each way ≈ 200ms RTT, got {r}"
    );
    lab.ok(&["set", "a", "b", "delay="]);
    let r = rtt(&lab, "a", "b");
    assert!((38.0..80.0).contains(&r), "back to 20ms each way, got {r}");
}

#[test]
fn timed_cuts_heal_themselves() {
    if !userns_available() {
        return;
    }
    let lab = Lab::new("timed", CHAIN);
    lab.up();
    lab.ok(&["cut", "a", "--for", "1s"]);
    assert!(!pings(&lab, "a", "b"));
    assert!(
        wait_until(Duration::from_secs(4), || pings(&lab, "a", "b")),
        "healed after 1s"
    );
}

#[test]
fn invalid_requests_change_nothing() {
    if !userns_available() {
        return;
    }
    let lab = Lab::new("invalid", CHAIN);
    lab.up();
    assert!(!lab.tpo(&["cut", "a", "c"]).status.success(), "no a–c link");
    assert!(
        !lab.tpo(&["cut", "a,b", "b"]).status.success(),
        "overlapping sides"
    );
    assert!(
        !lab.tpo(&["set", "a", "b", "delay=0", "jitter=5ms"])
            .status
            .success(),
        "jitter needs delay"
    );
    assert!(!lab.tpo(&["cut", "zz"]).status.success(), "unknown node");
    let show = lab.ok(&["show"]);
    assert!(!show.contains("CUT") && !show.contains('*'), "{show}");
}

#[test]
fn stop_and_start_node_processes() {
    if !userns_available() {
        return;
    }
    let lab = Lab::new("procs", &format!("{CHAIN}\n[run]\n\"*\" = \"sleep 600\"\n"));
    lab.up();
    let out = lab.ok(&["stop", "a,b", "--grace", "1s"]);
    assert!(
        out.contains("a: stopped") && out.contains("b: stopped"),
        "{out}"
    );
    let show = lab.ok(&["show"]);
    assert!(
        show.lines()
            .any(|l| l.starts_with("a ") && l.contains("stopped")),
        "{show}"
    );
    assert!(
        pings(&lab, "a", "b"),
        "stopping processes leaves the network up"
    );
    let out = lab.ok(&["start", "a"]);
    assert!(out.contains("a: started"), "{out}");
    let out = lab.ok(&["start", "c"]);
    assert!(out.contains("c: already running"), "{out}");
}

#[test]
fn apply_adds_and_removes_nodes_and_keeps_faults() {
    if !userns_available() {
        return;
    }
    let lab = Lab::new("apply", CHAIN);
    lab.up();
    lab.ok(&["cut", "a", "b"]);
    let next = lab.dir.path().join("next.toml");
    std::fs::write(
        &next,
        "name = \"apply\"\n[nodes]\na = {}\nb = {}\nd = {}\n[[link]]\npath = [\"a\", \"b\", \"d\"]\ndelay = \"20ms\"\n",
    )
    .unwrap();
    let out = lab.ok(&["apply", next.to_str().unwrap()]);
    assert!(out.contains("link b⇄d"), "{out}");
    assert!(out.contains("unlink b⇄c"), "{out}");
    assert!(pings(&lab, "b", "d"), "the new node is reachable by name");
    assert!(!pings(&lab, "a", "b"), "the a–b cut survives apply");
    assert!(
        lab.exec("c", &["true"]).status.code() != Some(0),
        "c is gone"
    );
    let show = lab.ok(&["show"]);
    assert!(
        show.contains("10.77.0.3") && show.lines().any(|l| l.starts_with("d ")),
        "d takes the freed address:\n{show}"
    );
}

#[test]
fn explain_prints_the_planned_commands() {
    if !userns_available() {
        return;
    }
    let lab = Lab::new("explain", CHAIN);
    lab.up();
    let out = lab.ok(&["explain", "a"]);
    assert!(
        out.contains("a: tc qdisc replace dev eth0 parent 1:2 netem limit 10000 delay 20000us"),
        "{out}"
    );
    assert!(out.contains("a: ip neigh replace 10.77.0.2"), "{out}");
}
