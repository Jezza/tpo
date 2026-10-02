//! Executes planned operations inside lab namespaces (spec §9 `backend`).

use crate::labdir::tool;
use crate::ns;
use crate::plan::ops::Action;
use crate::plan::ops::{HUB_PID, NETNS_OF};
use crate::plan::{Op, Target};
use nix::sched::CloneFlags;
use std::collections::HashMap;
use std::io::Write;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};

/// Absolute paths of the tools we run.
#[derive(Clone, Debug)]
pub struct Tools {
    pub ip: PathBuf,
    pub tc: PathBuf,
    pub sh: PathBuf,
    pub nft: Option<PathBuf>,
    pub pasta: Option<PathBuf>,
}

impl Tools {
    pub fn find() -> Result<Self, String> {
        let need = |name: &str, pkg: &str| {
            tool(name).ok_or_else(|| format!("`{name}` not found (install {pkg})"))
        };
        Ok(Tools {
            ip: need("ip", "iproute2")?,
            tc: need("tc", "iproute2")?,
            sh: need("sh", "a POSIX shell")?,
            nft: tool("nft"),
            pasta: tool("pasta"),
        })
    }
}

/// The network namespaces operations run in, plus the user namespace to join first.
pub struct NetFds {
    pub user: Option<OwnedFd>,
    pub hub_pid: i32,
    pub nets: HashMap<Target, OwnedFd>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Ip,
    Tc,
    Sysctl,
    Nft,
}

fn kind(op: &Op) -> Kind {
    match op.action {
        Action::Ip(_) => Kind::Ip,
        Action::Tc(_) => Kind::Tc,
        Action::Sysctl { .. } => Kind::Sysctl,
        Action::Nft(_) => Kind::Nft,
    }
}

/// Runs `ops` in order, batching consecutive operations for the same target and tool.
/// Stops at the first failure, reporting the failing operation and the tool's stderr.
pub fn run(fds: &NetFds, tools: &Tools, ops: &[Op]) -> Result<(), String> {
    let mut start = 0;
    while start < ops.len() {
        let (target, k) = (&ops[start].target, kind(&ops[start]));
        let end = ops[start..]
            .iter()
            .position(|op| &op.target != target || kind(op) != k || k == Kind::Nft)
            .map_or(ops.len(), |n| start + n.max(1));
        run_batch(fds, tools, &ops[start..end])?;
        start = end;
    }
    Ok(())
}

fn run_batch(fds: &NetFds, tools: &Tools, batch: &[Op]) -> Result<(), String> {
    let target = &batch[0].target;
    let net = fds
        .nets
        .get(target)
        .ok_or_else(|| format!("no namespace for `{target}`"))?
        .as_raw_fd();
    let user = fds.user.as_ref().map(AsRawFd::as_raw_fd);
    let hub_pid = fds.hub_pid.to_string();
    // `@netns:<target>` becomes an inherited fd for that target's netns.
    let mut inherited: Vec<RawFd> = Vec::new();
    let mut netns_path = |arg: &str| -> Result<String, String> {
        let t = Target::parse(&arg[NETNS_OF.len()..]);
        let fd = fds
            .nets
            .get(&t)
            .ok_or_else(|| format!("no namespace for `{t}`"))?
            .as_raw_fd();
        inherited.push(fd);
        Ok(format!("/proc/self/fd/{fd}"))
    };

    let (program, args, input): (&PathBuf, Vec<String>, String) = match kind(&batch[0]) {
        Kind::Ip | Kind::Tc => {
            let mut lines: Vec<String> = Vec::new();
            for op in batch {
                let (Action::Ip(a) | Action::Tc(a)) = &op.action else {
                    unreachable!()
                };
                let mut words = Vec::new();
                for w in a {
                    words.push(if w == HUB_PID {
                        hub_pid.clone()
                    } else if w.starts_with(NETNS_OF) {
                        netns_path(w)?
                    } else {
                        w.clone()
                    });
                }
                lines.push(words.join(" "));
            }
            let program = if kind(&batch[0]) == Kind::Ip {
                &tools.ip
            } else {
                &tools.tc
            };
            (
                program,
                vec!["-batch".into(), "-".into()],
                lines.join("\n") + "\n",
            )
        }
        Kind::Sysctl => {
            let script: Vec<String> = batch
                .iter()
                .map(|op| match &op.action {
                    Action::Sysctl { key, value } => {
                        format!("echo {value} > /proc/sys/{}", key.replace('.', "/"))
                    }
                    _ => unreachable!(),
                })
                .collect();
            (
                &tools.sh,
                vec!["-ec".into(), script.join("\n")],
                String::new(),
            )
        }
        Kind::Nft => {
            let nft = tools
                .nft
                .as_ref()
                .ok_or("`nft` not found (install nftables)")?;
            let Action::Nft(script) = &batch[0].action else {
                unreachable!()
            };
            (nft, vec!["-f".into(), "-".into()], script.clone())
        }
    };

    let inherited = inherited.clone();
    let mut cmd = Command::new(program);
    cmd.args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    // SAFETY: only setns calls between fork and exec.
    unsafe {
        cmd.pre_exec(move || {
            let mut joins: Vec<(RawFd, CloneFlags)> = Vec::new();
            if let Some(u) = user {
                joins.push((u, CloneFlags::CLONE_NEWUSER));
            }
            joins.push((net, CloneFlags::CLONE_NEWNET));
            ns::join(&joins)?;
            inherited.iter().try_for_each(|fd| ns::inherit(*fd))
        });
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("can't run {}: {e}", program.display()))?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(input.as_bytes());
    }
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    if out.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_owned();
    let failed = failed_line(&stderr)
        .and_then(|n| batch.get(n - 1))
        .unwrap_or(&batch[0]);
    Err(format!("`{failed}` failed: {stderr}"))
}

/// iproute2 reports `Command failed -:<line>` for batch input.
fn failed_line(stderr: &str) -> Option<usize> {
    let at = stderr.find("Command failed -:")?;
    stderr[at + 17..]
        .split(|c: char| !c.is_ascii_digit())
        .next()?
        .parse()
        .ok()
}
