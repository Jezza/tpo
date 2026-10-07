//! `ctl.sock`, signals and fault timers.

use super::{Shared, now_ms};
use crate::labdir::LabDir;
use crate::proto::{Envelope, Request, Response};
use nix::sys::socket::{ControlMessage, MsgFlags, sendmsg};
use std::io::{BufRead, BufReader, IoSlice, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub fn serve(shared: Shared) -> ! {
    let path = shared.lock().unwrap().dir.ctl();
    let listener = match UnixListener::bind(&path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("tpo: can't listen on {}: {e}", path.display());
            shutdown(&shared, 1);
        }
    };
    for stream in listener.incoming().flatten() {
        let shared = shared.clone();
        std::thread::spawn(move || handle(&shared, stream));
    }
    shutdown(&shared, 1)
}

fn handle(shared: &Shared, stream: UnixStream) {
    let mut line = String::new();
    if BufReader::new(&stream).read_line(&mut line).is_err() {
        return;
    }
    let request = match serde_json::from_str::<Envelope>(&line) {
        Ok(env) if env.v == crate::proto::VERSION => env.request,
        Ok(env) => {
            return reply(
                &stream,
                &Response::err(format!("unsupported protocol version {}", env.v)),
                &[],
            );
        }
        Err(e) => return reply(&stream, &Response::err(format!("bad request: {e}")), &[]),
    };
    match request {
        Request::Down => {
            reply(&stream, &Response::ok(vec![], serde_json::Value::Null), &[]);
            shutdown(shared, 0);
        }
        Request::Enter { node } => {
            let lab = shared.lock().unwrap();
            match lab.node_ns(&node) {
                Ok(ns) if lab.topo.nodes.contains_key(&node) => {
                    // The user namespace is absent in root mode, so name what we send.
                    let user = lab.init.user.as_ref().map(AsRawFd::as_raw_fd);
                    let (names, fds): (Vec<&str>, Vec<RawFd>) = [
                        ("user", user),
                        ("mnt", Some(ns.mnt.as_raw_fd())),
                        ("net", Some(ns.net.as_raw_fd())),
                        ("uts", Some(ns.uts.as_raw_fd())),
                        ("pid", Some(lab.init.pid_ns.as_raw_fd())),
                    ]
                    .into_iter()
                    .filter_map(|(name, fd)| fd.map(|fd| (name, fd)))
                    .unzip();
                    let data = serde_json::json!({ "env": lab.endpoint_env(&node), "fds": names });
                    reply(&stream, &Response::ok(vec![], data), &fds);
                }
                _ => reply(
                    &stream,
                    &Response::err(format!("unknown node `{node}`")),
                    &[],
                ),
            }
        }
        other => {
            let response = super::handlers::handle(shared, other);
            reply(&stream, &response, &[]);
        }
    }
}

fn reply(stream: &UnixStream, response: &Response, fds: &[RawFd]) {
    let mut line = serde_json::to_string(response).unwrap_or_else(|_| "{}".to_owned());
    line.push('\n');
    if fds.is_empty() {
        let _ = (&*stream).write_all(line.as_bytes());
        return;
    }
    let iov = [IoSlice::new(line.as_bytes())];
    let cmsg = [ControlMessage::ScmRights(fds)];
    let _ = sendmsg::<()>(stream.as_raw_fd(), &iov, &cmsg, MsgFlags::empty(), None);
}

fn shutdown(shared: &Shared, code: i32) -> ! {
    if let Ok(mut lab) = shared.lock() {
        lab.teardown();
    }
    std::process::exit(code)
}

/// Where the signal thread finds the lab once it exists.
pub type LabSlot = Arc<Mutex<Option<Shared>>>;

/// Handles Ctrl-C/SIGTERM/SIGHUP from the moment the lab init exists. Once the lab is up this is
/// a normal teardown. While it's still starting (or wedged holding the lock) the init is killed
/// directly: its pid namespace, and every lab process with it, dies with it.
pub fn spawn_signal_thread(slot: LabSlot, init: nix::unistd::Pid, dir: LabDir, root: bool) {
    use nix::sys::signal::{SigSet, Signal};
    std::thread::spawn(move || {
        let mut set = SigSet::empty();
        for s in [Signal::SIGINT, Signal::SIGTERM, Signal::SIGHUP] {
            set.add(s);
        }
        let _ = set.wait();
        let shared = slot.lock().unwrap().clone();
        if let Some(shared) = shared {
            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline {
                if let Ok(mut lab) = shared.try_lock() {
                    let started = !lab.starting;
                    lab.teardown();
                    std::process::exit(if started { 0 } else { 130 });
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        let _ = nix::sys::signal::kill(init, Signal::SIGKILL);
        if root {
            eprintln!(
                "tpo: interrupted while starting; clean up with `sudo tpo down --force --lab {}`",
                dir.name
            );
        } else {
            let _ = dir.remove();
            eprintln!("tpo: interrupted while starting");
        }
        std::process::exit(130);
    });
}

/// Expires timed faults (`--for`) and reconciles.
pub fn spawn_timer_thread(shared: Shared) {
    std::thread::spawn(move || {
        let wake = shared.lock().unwrap().wake.clone();
        let mut lab = shared.lock().unwrap();
        loop {
            if lab.shutting_down {
                return;
            }
            let now = now_ms();
            if lab.faults.next_deadline().is_some_and(|d| d <= now) {
                lab.faults.expire(now);
                if let Err(e) = lab.reconcile() {
                    eprintln!("tpo: reconciling expired faults failed: {e}");
                }
                lab.save();
            }
            let sleep = lab
                .faults
                .next_deadline()
                .map_or(Duration::from_secs(3600), |d| {
                    Duration::from_millis(d.saturating_sub(now_ms()))
                });
            lab = wake.wait_timeout(lab, sleep).unwrap().0;
        }
    });
}
