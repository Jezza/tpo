//! Helpers for integration tests that drive the real `tpo` binary.
//!
//! These tests need unprivileged user namespaces. When they aren't available the tests are
//! skipped with a message rather than reported as passing silently.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

pub struct Lab {
    pub dir: tempfile::TempDir,
    pub name: String,
}

pub fn userns_available() -> bool {
    let ok = Command::new("unshare")
        .args(["-Urn", "true"])
        .status()
        .is_ok_and(|s| s.success());
    if !ok {
        eprintln!("SKIPPED: unprivileged user namespaces are not available here");
    }
    ok
}

impl Lab {
    /// Writes `toml` as `<name>.toml` in a fresh runtime dir.
    pub fn new(name: &str, toml: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(format!("{name}.toml")), toml).unwrap();
        Lab {
            dir,
            name: name.to_owned(),
        }
    }

    pub fn file(&self) -> PathBuf {
        self.dir.path().join(format!("{}.toml", self.name))
    }

    pub fn runtime_dir(&self) -> PathBuf {
        self.dir.path().join("run")
    }

    pub fn lab_dir(&self) -> PathBuf {
        self.runtime_dir().join("tpo").join(&self.name)
    }

    pub fn tpo(&self, args: &[&str]) -> Output {
        std::fs::create_dir_all(self.runtime_dir()).unwrap();
        Command::new(env!("CARGO_BIN_EXE_tpo"))
            .args(args)
            .env("XDG_RUNTIME_DIR", self.runtime_dir())
            .current_dir(self.dir.path())
            .output()
            .unwrap()
    }

    /// Runs `tpo` and panics with its output unless it succeeds; returns stdout.
    pub fn ok(&self, args: &[&str]) -> String {
        let out = self.tpo(args);
        assert!(
            out.status.success(),
            "`tpo {}` failed ({}):\nstdout:\n{}\nstderr:\n{}",
            args.join(" "),
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    pub fn up(&self) {
        self.ok(&["up", "-d", self.file().to_str().unwrap()]);
    }

    pub fn exec(&self, node: &str, cmd: &[&str]) -> Output {
        let mut args = vec!["exec", node, "--"];
        args.extend_from_slice(cmd);
        self.tpo(&args)
    }

    pub fn supervisor_pid(&self) -> i32 {
        std::fs::read_to_string(self.lab_dir().join("pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap()
    }

    pub fn init_pid(&self) -> i32 {
        let state = std::fs::read_to_string(self.lab_dir().join("state.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&state).unwrap();
        v["init_pid"].as_i64().unwrap() as i32
    }
}

impl Drop for Lab {
    fn drop(&mut self) {
        if self.lab_dir().join("pid").exists() {
            let _ = self.tpo(&["down"]);
        }
    }
}

pub fn pid_alive(pid: i32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

pub fn wait_until(timeout: Duration, mut f: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    f()
}

/// The RTT in milliseconds from one `ping -c1` run's output.
pub fn ping_rtt_ms(out: &Output) -> Option<f64> {
    let text = String::from_utf8_lossy(&out.stdout);
    let at = text.find("time=")?;
    let rest = &text[at + 5..];
    rest.split_whitespace().next()?.parse().ok()
}
