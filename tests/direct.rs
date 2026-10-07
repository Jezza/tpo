//! Direct mode, end to end (spec §4.1, §5, §7.1).

mod common;

use common::*;
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

#[test]
fn linked_nodes_reach_each_other_by_name_over_ipv4_and_ipv6() {
    if !userns_available() {
        return;
    }
    let lab = Lab::new("reach", CHAIN);
    lab.up();
    for family in ["-4", "-6"] {
        let out = lab.exec("a", &["ping", family, "-c1", "-W2", "b"]);
        assert!(
            out.status.success(),
            "a→b {family}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

#[test]
fn links_add_the_configured_delay_in_each_direction() {
    if !userns_available() {
        return;
    }
    let lab = Lab::new("delay", CHAIN);
    lab.up();
    let out = lab.exec("a", &["ping", "-c1", "-W2", "b"]);
    let rtt = ping_rtt_ms(&out).expect("ping prints an RTT");
    assert!(
        (38.0..80.0).contains(&rtt),
        "20ms each way should give ≈40ms RTT, got {rtt}"
    );
}

#[test]
fn unlinked_nodes_exchange_nothing() {
    if !userns_available() {
        return;
    }
    let lab = Lab::new("isolate", CHAIN);
    lab.up();
    for family in ["-4", "-6"] {
        let out = lab.exec("a", &["ping", family, "-c1", "-W1", "c"]);
        assert!(!out.status.success(), "a must not reach c over {family}");
    }
    // IPv6 link-local multicast is answered by neighbours only.
    let out = lab.exec("b", &["ping", "-6", "-c2", "-W1", "ff02::1%eth0"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("fe80::1") && text.contains("fe80::3"),
        "b hears a and c:\n{text}"
    );
    let out = lab.exec("a", &["ping", "-6", "-c2", "-W1", "ff02::1%eth0"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(!text.contains("fe80::3"), "a must not hear c:\n{text}");
}

#[test]
fn exec_runs_inside_the_node() {
    if !userns_available() {
        return;
    }
    let lab = Lab::new("execns", CHAIN);
    lab.up();
    let out = lab.exec("b", &["hostname"]);
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "b");
    let out = lab.exec("b", &["sh", "-c", "echo $TPO_NODE $TPO_ADDR $TPO_ADDR_C"]);
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "b 10.77.0.2 10.77.0.3"
    );
    let out = lab.exec("b", &["sh", "-c", "exit 7"]);
    assert_eq!(
        out.status.code(),
        Some(7),
        "exec passes the exit status through"
    );
}

#[test]
fn run_commands_start_in_their_nodes_and_log() {
    if !userns_available() {
        return;
    }
    let lab = Lab::new(
        "runcmd",
        &format!(
            "{CHAIN}\n[run]\n\"*\" = \"echo hello from $TPO_NODE; sleep 600\"\n\"c\" = \"\"\n"
        ),
    );
    lab.up();
    let log = lab.lab_dir().join("logs").join("a.log");
    assert!(
        wait_until(Duration::from_secs(5), || std::fs::read_to_string(&log)
            .is_ok_and(|l| l.contains("hello from a"))),
        "a's run command logs"
    );
    let show = lab.ok(&["show"]);
    assert!(show.contains("running"), "{show}");
}

#[test]
fn down_leaves_nothing_behind() {
    if !userns_available() {
        return;
    }
    let lab = Lab::new(
        "teardown",
        &format!("{CHAIN}\n[run]\n\"*\" = \"sleep 600\"\n"),
    );
    lab.up();
    let (sup, init) = (lab.supervisor_pid(), lab.init_pid());
    lab.ok(&["down"]);
    assert!(wait_until(Duration::from_secs(5), || !pid_alive(sup)
        && !pid_alive(init)));
    assert!(!lab.lab_dir().exists(), "the lab dir is removed");
}

#[test]
fn killing_the_supervisor_kills_the_lab() {
    if !userns_available() {
        return;
    }
    let lab = Lab::new("crash", &format!("{CHAIN}\n[run]\n\"*\" = \"sleep 600\"\n"));
    lab.up();
    let (sup, init) = (lab.supervisor_pid(), lab.init_pid());
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(sup), nix::sys::signal::SIGKILL).unwrap();
    assert!(
        wait_until(Duration::from_secs(5), || !pid_alive(init)),
        "the lab init dies with the supervisor"
    );
    // A stale lab dir is cleaned up by the next command.
    lab.tpo(&["down"]);
    assert!(!lab.lab_dir().exists());
}

#[test]
fn node_processes_inherit_no_stray_fds() {
    if !userns_available() {
        return;
    }
    let lab = Lab::new(
        "fds",
        &format!(
            "{CHAIN}\n[run]\n\"a\" = \"ls /proc/self/fd | tr '\\\\n' ' '; echo; sleep 600\"\n"
        ),
    );
    lab.up();
    let log = lab.lab_dir().join("logs").join("a.log");
    assert!(wait_until(Duration::from_secs(5), || {
        std::fs::read_to_string(&log).is_ok_and(|l| l.contains('\n'))
    }));
    // ls itself opens fd 3 for the directory; nothing else may be open.
    assert_eq!(std::fs::read_to_string(&log).unwrap().trim(), "0 1 2 3");
}

/// Starts `tpo up` in the foreground with every node's setup slowed down.
fn slow_foreground_up(lab: &Lab, slow_ms: u64) -> std::process::Child {
    std::fs::create_dir_all(lab.runtime_dir()).unwrap();
    std::process::Command::new(env!("CARGO_BIN_EXE_tpo"))
        .args(["up", lab.file().to_str().unwrap()])
        .env("XDG_RUNTIME_DIR", lab.runtime_dir())
        .env("TPO_TEST_SLOW_START", slow_ms.to_string())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap()
}

/// Processes that belong to this lab (their command lines mention its runtime dir).
fn lab_processes(lab: &Lab) -> Vec<i32> {
    let marker = lab.runtime_dir().to_string_lossy().into_owned();
    std::fs::read_dir("/proc")
        .unwrap()
        .flatten()
        .filter_map(|e| e.file_name().to_string_lossy().parse::<i32>().ok())
        .filter(|pid| {
            std::fs::read(format!("/proc/{pid}/cmdline"))
                .is_ok_and(|c| String::from_utf8_lossy(&c).contains(&marker))
        })
        .collect()
}

#[test]
fn ctrl_c_during_startup_stops_the_lab() {
    if !userns_available() {
        return;
    }
    let lab = Lab::new("interrupt", CHAIN);
    let mut up = slow_foreground_up(&lab, 30_000);
    assert!(
        wait_until(Duration::from_secs(5), || !lab_processes(&lab).is_empty()),
        "startup began"
    );
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(up.id() as i32),
        nix::sys::signal::SIGINT,
    )
    .unwrap();
    assert!(
        wait_until(Duration::from_secs(5), || up.try_wait().unwrap().is_some()),
        "Ctrl-C stops a lab that is still starting"
    );
    assert!(
        wait_until(Duration::from_secs(5), || lab_processes(&lab).is_empty()),
        "{:?}",
        lab_processes(&lab)
    );
}

#[test]
fn a_node_that_never_starts_times_out() {
    if !userns_available() {
        return;
    }
    let lab = Lab::new("stuck", CHAIN);
    std::fs::create_dir_all(lab.runtime_dir()).unwrap();
    let start = std::time::Instant::now();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_tpo"))
        .args(["up", "-d", lab.file().to_str().unwrap()])
        .env("XDG_RUNTIME_DIR", lab.runtime_dir())
        .env("TPO_TEST_SLOW_START", "60000")
        .env("TPO_START_TIMEOUT", "1s")
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        start.elapsed() < Duration::from_secs(10),
        "took {:?}",
        start.elapsed()
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("didn't start within 1s"), "{err}");
    assert!(
        wait_until(Duration::from_secs(5), || lab_processes(&lab).is_empty()),
        "{:?}",
        lab_processes(&lab)
    );
}
