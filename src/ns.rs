//! Namespace primitives: the lab init (spec §7.1) and joining namespaces.

use nix::fcntl::{FcntlArg, FdFlag, fcntl};
use nix::sched::{CloneFlags, setns};
use nix::unistd::Pid;
use std::ffi::CString;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::path::Path;

fn io_err(e: nix::Error) -> io::Error {
    io::Error::from_raw_os_error(e as i32)
}

pub fn open_ns(pid: i32, kind: &str) -> io::Result<OwnedFd> {
    Ok(File::open(format!("/proc/{pid}/ns/{kind}"))?.into())
}

/// The lab init: pid 1 of the lab's pid namespace, in the lab's user and network namespaces.
pub struct LabInit {
    pub pid: Pid,
    pub user: OwnedFd,
    pub net: OwnedFd,
    pub pid_ns: OwnedFd,
    /// Closing this makes the init exit.
    _alive: OwnedFd,
}

/// Clones the lab init. Must be called while the process is single-threaded, from the thread
/// that will outlive the lab (the init's `PR_SET_PDEATHSIG` fires when that thread exits).
pub fn spawn_init(exe: &Path, rootless: bool) -> io::Result<LabInit> {
    let (read, write) = nix::unistd::pipe().map_err(io_err)?;
    // The init inherits `read` across exec; `write` stays with us.
    fcntl(&write, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC)).map_err(io_err)?;
    let read_fd = read.as_raw_fd();

    let exe = CString::new(exe.as_os_str().as_encoded_bytes()).map_err(io::Error::other)?;
    let args = [
        CString::new("tpo").unwrap(),
        CString::new("__init").unwrap(),
        CString::new(read_fd.to_string()).unwrap(),
    ];

    let mut flags = CloneFlags::CLONE_NEWNET | CloneFlags::CLONE_NEWPID;
    if rootless {
        flags |= CloneFlags::CLONE_NEWUSER;
    }
    let mut stack = vec![0u8; 256 * 1024];
    let child = Box::new(|| {
        let _ = nix::sys::prctl::set_pdeathsig(nix::sys::signal::Signal::SIGKILL);
        let _ = fcntl(
            unsafe { BorrowedFd::borrow_raw(read_fd) },
            FcntlArg::F_SETFD(FdFlag::empty()),
        );
        let _ = nix::unistd::execv(&exe, &args);
        127
    });
    // SAFETY: the caller guarantees we are single-threaded; the child only execs.
    let pid = unsafe { nix::sched::clone(child, &mut stack, flags, Some(libc_sigchld())) }
        .map_err(io_err)?;
    drop(read);

    if rootless {
        let uid = nix::unistd::getuid();
        let gid = nix::unistd::getgid();
        std::fs::write(format!("/proc/{pid}/uid_map"), format!("0 {uid} 1\n"))?;
        std::fs::write(format!("/proc/{pid}/setgroups"), "deny\n")?;
        std::fs::write(format!("/proc/{pid}/gid_map"), format!("0 {gid} 1\n"))?;
    }
    let raw = pid.as_raw();
    let init = LabInit {
        pid,
        user: open_ns(raw, "user")?,
        net: open_ns(raw, "net")?,
        pid_ns: open_ns(raw, "pid_for_children").or_else(|_| open_ns(raw, "pid"))?,
        _alive: write,
    };
    // Go: the maps are written.
    let mut w = File::from(init._alive.try_clone()?);
    w.write_all(b"g")?;
    Ok(init)
}

fn libc_sigchld() -> i32 {
    nix::sys::signal::Signal::SIGCHLD as i32
}

/// `tpo __init <fd>`: wait for the go-ahead, then reap zombies until the supervisor goes away.
pub fn run_init(fd: RawFd) -> ! {
    use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
    use nix::sys::wait::{WaitPidFlag, waitpid};

    // SAFETY: the fd was handed to us by the supervisor and is ours alone.
    let mut pipe = unsafe { File::from_raw_fd(fd) };
    let mut go = [0u8; 1];
    if !matches!(pipe.read(&mut go), Ok(1)) {
        std::process::exit(0);
    }
    loop {
        let mut fds = [PollFd::new(pipe.as_fd(), PollFlags::POLLIN)];
        let _ = poll(&mut fds, PollTimeout::from(500u16));
        if fds[0]
            .revents()
            .is_some_and(|r| r.intersects(PollFlags::POLLHUP | PollFlags::POLLIN))
        {
            // The supervisor closed its end (or died).
            std::process::exit(0);
        }
        while let Ok(status) = waitpid(Pid::from_raw(-1), Some(WaitPidFlag::WNOHANG)) {
            if status == nix::sys::wait::WaitStatus::StillAlive {
                break;
            }
        }
    }
}

/// Joins namespaces in order. Only valid in a single-threaded process (e.g. after `fork`).
pub fn join(nss: &[(RawFd, CloneFlags)]) -> io::Result<()> {
    for &(fd, kind) in nss {
        // SAFETY: the fds are owned by the caller for the duration of this call.
        setns(unsafe { BorrowedFd::borrow_raw(fd) }, kind).map_err(io_err)?;
    }
    Ok(())
}

/// Clears `FD_CLOEXEC` so an fd survives `exec`.
pub fn inherit(fd: RawFd) -> io::Result<()> {
    fcntl(
        unsafe { BorrowedFd::borrow_raw(fd) },
        FcntlArg::F_SETFD(FdFlag::empty()),
    )
    .map(|_| ())
    .map_err(io_err)
}
