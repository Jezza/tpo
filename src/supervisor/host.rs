//! Host-side state in root mode (spec §2, §6): named netns, forwarding sysctls, and cleaning up
//! after a supervisor that died (`tpo down --force`).

use super::kernel_nat::KernelNat;
use crate::labdir::{LabDir, running, tool};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;

/// What a root-mode lab changed on the host, recorded in `state.json` so `down --force` can undo
/// it without the supervisor.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct HostState {
    pub root: bool,
    /// `/run/netns` names attached for this lab.
    pub netns: Vec<String>,
    pub kernel_nat: Option<KernelNat>,
    /// `(sysctl path relative to the sysctl root, previous value)`, in the order they were set.
    pub sysctls: Vec<(String, String)>,
}

/// Writes sysctls under `root` (normally `/proc/sys`), remembering previous values.
pub struct Sysctls {
    pub root: PathBuf,
}

impl Sysctls {
    pub fn host() -> Self {
        Sysctls {
            root: PathBuf::from("/proc/sys"),
        }
    }

    fn read(&self, key: &str) -> Option<String> {
        std::fs::read_to_string(self.root.join(key))
            .ok()
            .map(|v| v.trim().to_owned())
    }

    fn set(&self, key: &str, value: &str, saved: &mut Vec<(String, String)>) -> Result<(), String> {
        let old = self
            .read(key)
            .ok_or_else(|| format!("can't read sysctl {key}"))?;
        if old == value {
            return Ok(());
        }
        std::fs::write(self.root.join(key), value)
            .map_err(|e| format!("can't set sysctl {key}: {e}"))?;
        saved.push((key.to_owned(), old));
        Ok(())
    }

    /// Enables IPv4/IPv6 forwarding. Interfaces that accept router advertisements get
    /// `accept_ra=2` first, so the host keeps its SLAAC default route once it forwards.
    pub fn enable_forwarding(&self) -> Result<Vec<(String, String)>, String> {
        let mut saved = Vec::new();
        let conf = self.root.join("net/ipv6/conf");
        let mut ifaces: Vec<String> = std::fs::read_dir(&conf)
            .map_err(|e| format!("can't list {}: {e}", conf.display()))?
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| !["all", "default", "lo"].contains(&n.as_str()))
            .collect();
        ifaces.sort();
        for iface in ifaces {
            let key = format!("net/ipv6/conf/{iface}/accept_ra");
            if self.read(&key).as_deref() == Some("1") {
                self.set(&key, "2", &mut saved)?;
            }
        }
        self.set("net/ipv4/ip_forward", "1", &mut saved)?;
        self.set("net/ipv6/conf/all/forwarding", "1", &mut saved)?;
        Ok(saved)
    }

    /// Restores saved values, newest first.
    pub fn restore(&self, saved: &[(String, String)]) {
        for (key, value) in saved.iter().rev() {
            if let Err(e) = std::fs::write(self.root.join(key), value) {
                eprintln!("tpo: can't restore sysctl {key}: {e}");
            }
        }
    }
}

fn ip(args: &[&str]) -> Result<String, String> {
    let ip = tool("ip").ok_or("`ip` not found")?;
    let out = Command::new(ip)
        .args(args)
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(format!(
            "`ip {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

pub fn netns_name(lab: &str, endpoint: &str) -> String {
    format!("tpo-{lab}-{endpoint}")
}

/// Makes a holder's netns visible as `/run/netns/<name>` (while the holder is alive).
pub fn attach_netns(name: &str, pid: i32) -> Result<(), String> {
    ip(&["netns", "attach", name, &pid.to_string()]).map(|_| ())
}

/// Kills whatever still runs in a named netns and deletes it.
pub fn delete_netns(name: &str) {
    if !Path::new("/run/netns").join(name).exists() {
        return;
    }
    if let Ok(pids) = ip(&["netns", "pids", name]) {
        for pid in pids.split_whitespace().filter_map(|p| p.parse().ok()) {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(pid),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
    }
    if let Err(e) = ip(&["netns", "del", name]) {
        eprintln!("tpo: {e}");
    }
}

/// Whether another running lab (not `except`) uses `--kernel-nat`.
pub fn other_kernel_nat_lab(except: &str) -> bool {
    running()
        .iter()
        .filter(|l| l.name != except)
        .any(|l| load(l).is_some_and(|h| h.kernel_nat.is_some()))
}

/// The host state recorded in a lab dir's `state.json`.
pub fn load(dir: &LabDir) -> Option<HostState> {
    let text = std::fs::read_to_string(dir.state()).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    serde_json::from_value(v.get("host")?.clone()).ok()
}

/// The pasta recorded in `state.json`, if that pid is still a pasta process.
fn recorded_pasta(dir: &LabDir) -> Option<i32> {
    let text = std::fs::read_to_string(dir.state()).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    let pid = i32::try_from(v["pasta_pid"].as_i64()?).ok()?;
    let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    String::from_utf8_lossy(&cmdline)
        .contains("pasta")
        .then_some(pid)
}

/// Undoes everything a root-mode lab changed on the host, for a lab whose supervisor is gone.
pub fn force_down(dir: &LabDir) -> Result<Vec<String>, String> {
    let mut done = Vec::new();
    if let Some(pid) = recorded_pasta(dir) {
        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(pid),
            nix::sys::signal::Signal::SIGTERM,
        );
        done.push(format!("stopped pasta (pid {pid})"));
    }
    if let Some(state) = load(dir) {
        for name in &state.netns {
            delete_netns(name);
            done.push(format!("deleted netns {name}"));
        }
        if let Some(k) = &state.kernel_nat {
            let _ = ip(&["link", "del", &k.host_if]);
            if let Some(nft) = tool("nft") {
                let _ = Command::new(nft)
                    .args(["delete", "table", "inet", &k.table])
                    .output();
            }
            done.push(format!("removed {} and nft table {}", k.host_if, k.table));
            if !other_kernel_nat_lab(&dir.name) {
                Sysctls::host().restore(&state.sysctls);
                done.push("restored forwarding sysctls".to_owned());
            }
        }
    }
    dir.remove().map_err(|e| e.to_string())?;
    done.push(format!("removed {}", dir.path.display()));
    Ok(done)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_proc(entries: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (key, value) in entries {
            let path = dir.path().join(key);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, format!("{value}\n")).unwrap();
        }
        dir
    }

    #[test]
    fn forwarding_keeps_slaac_by_switching_accept_ra_first() {
        let root = fake_proc(&[
            ("net/ipv4/ip_forward", "0"),
            ("net/ipv6/conf/all/forwarding", "0"),
            ("net/ipv6/conf/all/accept_ra", "1"),
            ("net/ipv6/conf/lo/accept_ra", "1"),
            ("net/ipv6/conf/wlan0/accept_ra", "1"),
            ("net/ipv6/conf/eth9/accept_ra", "0"),
        ]);
        let s = Sysctls {
            root: root.path().to_owned(),
        };
        let saved = s.enable_forwarding().unwrap();
        assert_eq!(
            saved,
            [
                ("net/ipv6/conf/wlan0/accept_ra".to_owned(), "1".to_owned()),
                ("net/ipv4/ip_forward".to_owned(), "0".to_owned()),
                ("net/ipv6/conf/all/forwarding".to_owned(), "0".to_owned()),
            ]
        );
        let read = |k: &str| {
            std::fs::read_to_string(root.path().join(k))
                .unwrap()
                .trim()
                .to_owned()
        };
        assert_eq!(read("net/ipv6/conf/wlan0/accept_ra"), "2");
        assert_eq!(read("net/ipv6/conf/eth9/accept_ra"), "0");
        s.restore(&saved);
        assert_eq!(read("net/ipv6/conf/wlan0/accept_ra"), "1");
        assert_eq!(read("net/ipv4/ip_forward"), "0");
    }

    #[test]
    fn values_that_are_already_set_are_not_recorded() {
        let root = fake_proc(&[
            ("net/ipv4/ip_forward", "1"),
            ("net/ipv6/conf/all/forwarding", "1"),
        ]);
        let s = Sysctls {
            root: root.path().to_owned(),
        };
        assert!(s.enable_forwarding().unwrap().is_empty());
    }
}
