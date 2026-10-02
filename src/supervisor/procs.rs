//! Node processes from `[run]` (spec §3.5, §7.4).

use super::{Lab, Shared};
use crate::enter::EnterSpec;
use serde::Serialize;
use std::fs::File;
use std::os::fd::AsRawFd;
use std::process::Stdio;
use std::time::{Duration, Instant};

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ProcState {
    Running { pid: i32 },
    Exited { code: i32 },
    Stopped,
}

#[derive(Clone, Debug, Serialize)]
pub struct Proc {
    pub cmd: String,
    #[serde(flatten)]
    pub state: ProcState,
    /// Bumped on every start, so a stale exit notification can't overwrite a newer process.
    #[serde(skip)]
    pub generation: u64,
}

pub fn start_all(shared: &Shared) {
    let nodes: Vec<String> = shared.lock().unwrap().topo.nodes.keys().cloned().collect();
    for node in nodes {
        let _ = start(shared, &node);
    }
}

/// Starts `node`'s run command; returns a per-node status line.
pub fn start(shared: &Shared, node: &str) -> Result<String, String> {
    let mut lab = shared.lock().unwrap();
    let Some(cmd) = lab.topo.run_command(node).map(str::to_owned) else {
        return Ok(format!("{node}: no command"));
    };
    if let Some(Proc {
        state: ProcState::Running { .. },
        ..
    }) = lab.procs.get(node)
    {
        return Ok(format!("{node}: already running"));
    }
    let (child, pid) = spawn(&lab, node, &cmd)?;
    let generation = lab.procs.get(node).map_or(0, |p| p.generation + 1);
    lab.procs.insert(
        node.to_owned(),
        Proc {
            cmd,
            state: ProcState::Running { pid },
            generation,
        },
    );
    drop(lab);

    let shared = shared.clone();
    let node_name = node.to_owned();
    std::thread::spawn(move || {
        let mut child = child;
        let code = child.wait().ok().and_then(|s| s.code()).unwrap_or(-1);
        let mut lab = shared.lock().unwrap();
        if let Some(p) = lab.procs.get_mut(&node_name)
            && p.generation == generation
            && matches!(p.state, ProcState::Running { .. })
        {
            p.state = ProcState::Exited { code };
        }
    });
    Ok(format!("{node}: started (pid {pid})"))
}

fn spawn(lab: &Lab, node: &str, cmd: &str) -> Result<(std::process::Child, i32), String> {
    let ns = lab.node_ns(node)?;
    let log = File::options()
        .create(true)
        .append(true)
        .open(lab.dir.log(node))
        .map_err(|e| format!("can't open {node}'s log: {e}"))?;
    let log2 = log.try_clone().map_err(|e| e.to_string())?;
    let spec = EnterSpec {
        user: Some(lab.init.user.as_raw_fd()),
        mnt: Some(ns.mnt.as_raw_fd()),
        net: Some(ns.net.as_raw_fd()),
        uts: Some(ns.uts.as_raw_fd()),
        pid: Some(lab.init.pid_ns.as_raw_fd()),
        setpgid: true,
        cwd: lab.file.parent().map(ToOwned::to_owned),
        env: lab.endpoint_env(node),
        cmd: vec!["/bin/sh".to_owned(), "-c".to_owned(), cmd.to_owned()],
        ..EnterSpec::default()
    };
    lab.spawn_enter(spec, Stdio::null(), Stdio::from(log), Stdio::from(log2))
}

/// Signals `node`'s process group and waits up to `grace` before SIGKILL.
pub fn stop(
    shared: &Shared,
    node: &str,
    signal: nix::sys::signal::Signal,
    grace: Duration,
) -> String {
    use nix::sys::signal::{Signal, killpg};
    use nix::unistd::Pid;
    let pid = {
        let mut lab = shared.lock().unwrap();
        match lab.procs.get_mut(node) {
            Some(p) => match p.state {
                ProcState::Running { pid } => {
                    p.state = ProcState::Stopped;
                    pid
                }
                _ => return format!("{node}: not running"),
            },
            None => return format!("{node}: no command"),
        }
    };
    let group = Pid::from_raw(pid);
    let _ = killpg(group, signal);
    let deadline = Instant::now() + grace;
    while Instant::now() < deadline {
        if killpg(group, None).is_err() {
            return format!("{node}: stopped");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = killpg(group, Signal::SIGKILL);
    format!("{node}: killed after {}s", grace.as_secs())
}
