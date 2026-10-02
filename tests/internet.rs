//! Internet access through the gateway (spec §6). Needs pasta and host internet connectivity.

mod common;

use common::*;
use std::process::{Command, Stdio};
use std::time::Duration;

fn internet_available() -> bool {
    if !userns_available() {
        return false;
    }
    let pasta = Command::new("pasta")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success());
    let online = Command::new("curl")
        .args(["-s", "-m5", "-o/dev/null", "http://1.1.1.1/"])
        .status()
        .is_ok_and(|s| s.success());
    if !(pasta && online) {
        eprintln!("SKIPPED: needs pasta and internet access (pasta={pasta}, online={online})");
    }
    pasta && online
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

fn curl(lab: &Lab, node: &str, args: &[&str]) -> bool {
    let mut cmd = vec!["curl", "-s", "-m8", "-o/dev/null"];
    cmd.extend_from_slice(args);
    lab.exec(node, &cmd).status.success()
}

const LAB: &str = r#"
[nodes]
a = {}
b = {}
c = {}

[[link]]
from = ["a", "b"]
to = "internet"
delay = "5ms"
"#;

#[test]
fn linked_nodes_reach_the_internet_by_ip_and_by_name() {
    if !internet_available() {
        return;
    }
    let lab = Lab::new("inet", LAB);
    lab.up();
    assert!(curl(&lab, "a", &["http://1.1.1.1/"]), "a reaches 1.1.1.1");
    assert!(
        curl(&lab, "a", &["https://cloudflare.com/"]),
        "a resolves and reaches a name over HTTPS"
    );
    if host_has_ipv6() {
        assert!(
            curl(&lab, "a", &["-6", "http://[2606:4700:4700::1111]/"]),
            "a reaches the internet over IPv6"
        );
        assert!(
            curl(&lab, "a", &["-6", "https://cloudflare.com/"]),
            "and by name over IPv6"
        );
    }
    assert!(
        !curl(&lab, "c", &["http://1.1.1.1/"]),
        "c has no internet link"
    );
}

#[test]
fn nodes_sharing_the_internet_stay_isolated_from_each_other() {
    if !internet_available() {
        return;
    }
    let lab = Lab::new("inetiso", LAB);
    lab.up();
    for family in ["-4", "-6"] {
        let out = lab.exec("a", &["ping", family, "-c1", "-W1", "b"]);
        assert!(
            !out.status.success(),
            "a must not reach b over {family} through the gateway"
        );
    }
    let out = lab.exec("c", &["ping", "-c1", "-W1", "internet"]);
    assert!(!out.status.success(), "c can't even ping the gateway");
}

#[test]
fn cutting_the_internet_link_kills_open_connections() {
    if !internet_available() {
        return;
    }
    let lab = Lab::new("inetcut", LAB);
    lab.up();
    // Connect, wait while the link is cut, then try to use the connection.
    let script = "import socket, time\n\
                  s = socket.create_connection(('1.1.1.1', 80), timeout=5)\n\
                  print('connected', flush=True)\n\
                  time.sleep(2)\n\
                  s.settimeout(3)\n\
                  s.sendall(b'GET / HTTP/1.1\\r\\nHost: 1.1.1.1\\r\\n\\r\\n')\n\
                  print('answer' if s.recv(100) else 'closed', flush=True)\n";
    let child = Command::new(env!("CARGO_BIN_EXE_tpo"))
        .args(["exec", "a", "--", "python3", "-c", script])
        .env("XDG_RUNTIME_DIR", lab.runtime_dir())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(1000));
    lab.ok(&["cut", "a", "internet"]);
    let out = child.wait_with_output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("connected"),
        "the connection was opened first: {text}"
    );
    assert!(
        !text.contains("answer"),
        "the open connection must stop working: {text}"
    );
    assert!(!curl(&lab, "a", &["http://1.1.1.1/"]));
    lab.ok(&["heal", "a", "internet"]);
    assert!(curl(&lab, "a", &["http://1.1.1.1/"]));
}

#[test]
fn host_loopback_reaches_services_on_the_host() {
    if !internet_available() {
        return;
    }
    let mut server = Command::new("python3")
        .args(["-m", "http.server", "--bind", "127.0.0.1", "18731"])
        .current_dir(std::env::temp_dir())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(500));
    let lab = Lab::new(
        "inethost",
        &format!("{LAB}\n[internet]\nhost_loopback = true\n"),
    );
    lab.up();
    let ok = curl(&lab, "a", &["http://host:18731/"]);
    let _ = server.kill();
    let _ = server.wait();
    assert!(ok, "a reaches the host's 127.0.0.1:18731 as `host`");
}

#[test]
fn routed_labs_reach_the_internet_through_linked_nodes() {
    if !internet_available() {
        return;
    }
    let lab = Lab::new(
        "inetrouted",
        "mode = \"routed\"\n[nodes]\na = {}\nb = {}\n[[link]]\npath = [\"a\", \"b\", \"internet\"]\n",
    );
    lab.up();
    assert!(
        curl(&lab, "b", &["http://1.1.1.1/"]),
        "b is linked to internet"
    );
    assert!(
        curl(&lab, "a", &["http://1.1.1.1/"]),
        "a reaches it through b"
    );
}

#[test]
fn killing_the_supervisor_leaves_no_pasta_behind() {
    if !internet_available() {
        return;
    }
    let lab = Lab::new("inetcrash", LAB);
    lab.up();
    let state = std::fs::read_to_string(lab.lab_dir().join("state.json")).unwrap();
    let v: serde_json::Value = serde_json::from_str(&state).unwrap();
    let pasta = v["pasta_pid"].as_i64().expect("state records pasta's pid") as i32;
    assert!(pid_alive(pasta));
    let sup = lab.supervisor_pid();
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(sup), nix::sys::signal::SIGKILL).unwrap();
    assert!(
        wait_until(Duration::from_secs(10), || !pid_alive(pasta)),
        "pasta exits with the lab"
    );
}
