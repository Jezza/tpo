//! Entering a node: `tpo __enter`, node processes and `tpo exec`/`shell` (spec §7.1, §7.2).

use crate::ns;
use nix::mount::{MsFlags, mount};
use nix::sched::{CloneFlags, unshare};
use nix::sys::signal::{SaFlags, SigAction, SigHandler, SigSet, Signal, sigaction};
use nix::sys::wait::{WaitStatus, waitpid};
use nix::unistd::{ForkResult, Pid, fork};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{FromRawFd, RawFd};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicI32, Ordering};

/// Builds a fresh node: new net, mount and UTS namespaces, `/proc` for the lab pid namespace,
/// bind-mounted `/etc` files and the hostname (spec §5).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct NodeSetup {
    pub hostname: String,
    /// `(source, target)` bind mounts.
    pub binds: Vec<(PathBuf, PathBuf)>,
}

/// What to enter and run. Namespace fields are inherited fds, joined in field order.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct EnterSpec {
    pub user: Option<RawFd>,
    pub mnt: Option<RawFd>,
    pub net: Option<RawFd>,
    pub uts: Option<RawFd>,
    pub pid: Option<RawFd>,
    pub setup: Option<NodeSetup>,
    /// Receives `pid <host pid>` from the wrapper and `ready` from the child.
    pub report: Option<RawFd>,
    /// Instead of running a command, wait until stdin closes (a namespace holder).
    pub hold: bool,
    /// Put the child in its own process group (node processes, so `stop` can signal it).
    pub setpgid: bool,
    /// An interactive session: SIGINT/SIGQUIT reach the child via the tty already.
    pub interactive: bool,
    pub cwd: Option<PathBuf>,
    pub env: Vec<(String, String)>,
    pub cmd: Vec<String>,
}

/// Writes one line of the report pipe with a single `write` call. The wrapper and the child
/// write to the same pipe concurrently, and only single writes (up to `PIPE_BUF`) are atomic;
/// `writeln!` on an unbuffered file issues one write per formatted piece and can interleave.
fn report_line(w: &mut impl Write, line: &str) {
    let _ = w.write_all(format!("{line}\n").as_bytes());
}

static CHILD: AtomicI32 = AtomicI32::new(0);

extern "C" fn forward(sig: i32) {
    let child = CHILD.load(Ordering::SeqCst);
    if child > 0 {
        // SAFETY: kill is async-signal-safe.
        unsafe { nix::libc::kill(child, sig) };
    }
}

fn fail(msg: impl std::fmt::Display) -> ! {
    eprintln!("tpo: {msg}");
    std::process::exit(126);
}

/// Enters the namespaces in `spec` and runs (or holds) there; exits with the child's status.
/// The calling process must be single-threaded.
pub fn enter(spec: &EnterSpec) -> ! {
    let original_cwd = std::env::current_dir().ok();
    let joins: Vec<(RawFd, CloneFlags)> = [
        (spec.user, CloneFlags::CLONE_NEWUSER),
        (spec.mnt, CloneFlags::CLONE_NEWNS),
        (spec.net, CloneFlags::CLONE_NEWNET),
        (spec.uts, CloneFlags::CLONE_NEWUTS),
        (spec.pid, CloneFlags::CLONE_NEWPID),
    ]
    .into_iter()
    .filter_map(|(fd, kind)| fd.map(|fd| (fd, kind)))
    .collect();
    if let Err(e) = ns::join(&joins) {
        fail(format!("can't enter the node's namespaces: {e}"));
    }
    let cwd = spec.cwd.clone().or(original_cwd);
    if cwd
        .as_ref()
        .is_none_or(|d| std::env::set_current_dir(d).is_err())
    {
        let _ = std::env::set_current_dir("/");
    }

    let mut report = spec.report.map(|fd| unsafe { File::from_raw_fd(fd) });

    // SAFETY: single-threaded (see above), so fork is sound.
    match unsafe { fork() } {
        Err(e) => fail(format!("fork failed: {e}")),
        Ok(ForkResult::Child) => child(spec, report.as_mut()),
        Ok(ForkResult::Parent { child }) => {
            CHILD.store(child.as_raw(), Ordering::SeqCst);
            if let Some(r) = report.as_mut() {
                report_line(r, &format!("pid {child}"));
            }
            drop(report);
            let handler = SigAction::new(
                SigHandler::Handler(forward),
                SaFlags::SA_RESTART,
                SigSet::empty(),
            );
            let ignore = SigAction::new(SigHandler::SigIgn, SaFlags::empty(), SigSet::empty());
            // SAFETY: the handler only calls kill.
            unsafe {
                let _ = sigaction(Signal::SIGTERM, &handler);
                let _ = sigaction(Signal::SIGHUP, &handler);
                let interrupt = if spec.interactive { &ignore } else { &handler };
                let _ = sigaction(Signal::SIGINT, interrupt);
                let _ = sigaction(Signal::SIGQUIT, interrupt);
            }
            std::process::exit(wait_for(child));
        }
    }
}

fn wait_for(child: Pid) -> i32 {
    loop {
        match waitpid(child, None) {
            Ok(WaitStatus::Exited(_, code)) => return code,
            Ok(WaitStatus::Signaled(_, sig, _)) => return 128 + sig as i32,
            Ok(_) | Err(nix::Error::EINTR) => continue,
            Err(e) => fail(format!("waitpid failed: {e}")),
        }
    }
}

fn child(spec: &EnterSpec, report: Option<&mut File>) -> ! {
    if let Some(setup) = &spec.setup
        && let Err(e) = build_node(setup)
    {
        fail(format!("can't set up node `{}`: {e}", setup.hostname));
    }
    if spec.setpgid {
        let _ = nix::unistd::setpgid(Pid::from_raw(0), Pid::from_raw(0));
    }
    if let Some(r) = report {
        report_line(r, "ready");
    }
    if spec.hold {
        let mut sink = Vec::new();
        let _ = std::io::stdin().read_to_end(&mut sink);
        std::process::exit(0);
    }
    let Some((program, args)) = spec.cmd.split_first() else {
        fail("no command given")
    };
    let err = Command::new(program)
        .args(args)
        .envs(spec.env.iter().cloned())
        .exec();
    eprintln!("tpo: can't run `{program}`: {err}");
    std::process::exit(if err.kind() == std::io::ErrorKind::NotFound {
        127
    } else {
        126
    });
}

fn build_node(setup: &NodeSetup) -> nix::Result<()> {
    unshare(CloneFlags::CLONE_NEWNET | CloneFlags::CLONE_NEWNS | CloneFlags::CLONE_NEWUTS)?;
    mount(
        None::<&str>,
        "/",
        None::<&str>,
        MsFlags::MS_REC | MsFlags::MS_PRIVATE,
        None::<&str>,
    )?;
    mount(
        Some("proc"),
        "/proc",
        Some("proc"),
        MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_NOEXEC,
        None::<&str>,
    )?;
    for (source, target) in &setup.binds {
        mount(
            Some(source.as_path()),
            target.as_path(),
            None::<&str>,
            MsFlags::MS_BIND,
            None::<&str>,
        )?;
    }
    nix::unistd::sethostname(&setup.hostname)?;
    Ok(())
}

/// Spawns `exe __enter` with `spec`'s namespace fds inherited; returns the wrapper and the host
/// pid of the process inside, once it reported `ready`.
pub fn spawn(
    exe: &std::path::Path,
    mut spec: EnterSpec,
    stdin: std::process::Stdio,
    stdout: std::process::Stdio,
    stderr: std::process::Stdio,
) -> Result<(std::process::Child, i32), String> {
    use std::io::BufRead;
    use std::os::fd::AsRawFd;
    let (read, write) = nix::unistd::pipe().map_err(|e| e.to_string())?;
    spec.report = Some(write.as_raw_fd());
    let inherit: Vec<RawFd> = [
        spec.user,
        spec.mnt,
        spec.net,
        spec.uts,
        spec.pid,
        spec.report,
    ]
    .into_iter()
    .flatten()
    .collect();
    let json = serde_json::to_string(&spec).map_err(|e| e.to_string())?;
    let mut cmd = Command::new(exe);
    cmd.arg("__enter")
        .arg(json)
        .stdin(stdin)
        .stdout(stdout)
        .stderr(stderr);
    // SAFETY: only fcntl between fork and exec.
    unsafe {
        cmd.pre_exec(move || inherit.iter().try_for_each(|fd| ns::inherit(*fd)));
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("can't start `tpo __enter`: {e}"))?;
    drop(write);

    let (mut pid, mut ready) = (None, false);
    for line in std::io::BufReader::new(File::from(read)).lines() {
        let Ok(line) = line else { break };
        if let Some(p) = line.strip_prefix("pid ") {
            pid = p.trim().parse().ok();
        } else if line == "ready" {
            ready = true;
        }
        if pid.is_some() && ready {
            break;
        }
    }
    match (pid, ready) {
        (Some(pid), true) => Ok((child, pid)),
        _ => {
            let mut why = String::new();
            if let Some(mut err) = child.stderr.take() {
                let _ = std::io::Read::read_to_string(&mut err, &mut why);
            }
            let _ = child.wait();
            let why = why.trim();
            Err(if why.is_empty() {
                "a node process failed to start".to_owned()
            } else {
                why.to_owned()
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Records each `write` call separately.
    #[derive(Default)]
    struct Calls(Vec<Vec<u8>>);

    impl Write for Calls {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.push(buf.to_vec());
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn report_lines_are_single_writes_so_writers_cannot_interleave() {
        // The wrapper ("pid …") and the child ("ready") share the report pipe. Pipe writes up to
        // PIPE_BUF are atomic, but only if each line is one write call.
        let mut w = Calls::default();
        report_line(&mut w, &format!("pid {}", 143296));
        report_line(&mut w, "ready");
        assert_eq!(w.0, [b"pid 143296\n".to_vec(), b"ready\n".to_vec()]);
    }
}
