//! Lab directories: `$XDG_RUNTIME_DIR/tpo/<lab>/` (spec §7.1).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

pub fn runtime_root() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) => PathBuf::from(dir).join("tpo"),
        // `sudo` usually drops XDG_RUNTIME_DIR; root labs live in /run/tpo.
        None if nix::unistd::geteuid().is_root() => PathBuf::from("/run/tpo"),
        None => std::env::temp_dir()
            .join(format!("tpo-{}", nix::unistd::getuid()))
            .join("tpo"),
    }
}

/// Whether a dead lab's dir records root-mode host state that `down --force` must undo.
fn has_root_leftovers(lab: &LabDir) -> bool {
    std::fs::read_to_string(lab.state())
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .is_some_and(|v| v["host"]["root"] == serde_json::Value::Bool(true))
}

/// Dead root-mode labs whose leftovers need `tpo down --force`.
pub fn stale() -> Vec<LabDir> {
    let Ok(entries) = fs::read_dir(runtime_root()) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|e| e.path().is_dir())
        .map(|e| LabDir::new(&e.file_name().to_string_lossy()))
        .filter(|lab| lab.live_pid().is_none() && has_root_leftovers(lab))
        .collect()
}

#[derive(Clone, Debug)]
pub struct LabDir {
    pub name: String,
    pub path: PathBuf,
}

impl LabDir {
    pub fn new(name: &str) -> Self {
        LabDir {
            name: name.to_owned(),
            path: runtime_root().join(name),
        }
    }

    pub fn pid_file(&self) -> PathBuf {
        self.path.join("pid")
    }
    pub fn ctl(&self) -> PathBuf {
        self.path.join("ctl.sock")
    }
    pub fn state(&self) -> PathBuf {
        self.path.join("state.json")
    }
    pub fn logs(&self) -> PathBuf {
        self.path.join("logs")
    }
    pub fn log(&self, node: &str) -> PathBuf {
        self.logs().join(format!("{node}.log"))
    }
    /// The supervisor's own log, next to the lab dir so it outlives the lab.
    pub fn supervisor_log(&self) -> PathBuf {
        runtime_root().join(format!("{}.log", self.name))
    }
    pub fn etc(&self) -> PathBuf {
        self.path.join("etc")
    }

    /// The supervisor's pid, if it is still running.
    pub fn live_pid(&self) -> Option<i32> {
        let pid: i32 = fs::read_to_string(self.pid_file())
            .ok()?
            .trim()
            .parse()
            .ok()?;
        let cmdline = fs::read(format!("/proc/{pid}/cmdline")).ok()?;
        String::from_utf8_lossy(&cmdline)
            .contains("tpo")
            .then_some(pid)
    }

    pub fn exists(&self) -> bool {
        self.path.exists()
    }

    pub fn remove(&self) -> io::Result<()> {
        match fs::remove_dir_all(&self.path) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }
}

/// Running labs; stale lab dirs (dead supervisor) are removed along the way.
pub fn running() -> Vec<LabDir> {
    let Ok(entries) = fs::read_dir(runtime_root()) else {
        return Vec::new();
    };
    let mut labs: Vec<LabDir> = entries
        .flatten()
        .filter(|e| e.path().is_dir())
        .map(|e| LabDir::new(&e.file_name().to_string_lossy()))
        .filter(|lab| {
            if lab.live_pid().is_some() {
                true
            } else {
                // Root-mode leftovers wait for `tpo down --force`.
                if !has_root_leftovers(lab) {
                    let _ = lab.remove();
                }
                false
            }
        })
        .collect();
    labs.sort_by(|a, b| a.name.cmp(&b.name));
    labs
}

/// The lab named `name`, or the only running lab.
pub fn find(name: Option<&str>) -> Result<LabDir, String> {
    let labs = running();
    match name {
        Some(n) => labs
            .into_iter()
            .find(|l| l.name == n)
            .ok_or_else(|| format!("lab `{n}` is not running")),
        None => match labs.as_slice() {
            [] => Err("no lab is running (start one with `tpo up <file>`)".to_owned()),
            [one] => Ok(one.clone()),
            many => {
                let names: Vec<_> = many.iter().map(|l| l.name.as_str()).collect();
                Err(format!(
                    "several labs are running ({}); pick one with --lab",
                    names.join(", ")
                ))
            }
        },
    }
}

/// Resolves a tool to an absolute path, looking in the sbin directories first.
pub fn tool(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    ["/usr/sbin", "/sbin", "/usr/bin", "/bin"]
        .into_iter()
        .map(PathBuf::from)
        .chain(std::env::split_paths(&path))
        .map(|dir| dir.join(name))
        .find(|p| is_executable(p))
}

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}
