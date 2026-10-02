//! A blocking `ctl.sock` client (spec §9 `tpo::Client`).

use crate::labdir::{self, LabDir};
use crate::proto::{Envelope, Request, Response, VERSION};
use nix::sys::socket::{ControlMessageOwned, MsgFlags, recvmsg};
use std::io::{BufRead, BufReader, IoSliceMut, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::time::Duration;

pub struct Client {
    pub lab: LabDir,
    timeout: Duration,
}

impl Client {
    /// Connects to the lab named `name`, or the only running lab.
    pub fn connect(name: Option<&str>) -> Result<Self, String> {
        Ok(Client {
            lab: labdir::find(name)?,
            timeout: Duration::from_secs(10),
        })
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    fn stream(&self) -> Result<UnixStream, String> {
        let s = UnixStream::connect(self.lab.ctl())
            .map_err(|e| format!("lab `{}` is not reachable: {e}", self.lab.name))?;
        s.set_read_timeout(Some(self.timeout))
            .map_err(|e| e.to_string())?;
        Ok(s)
    }

    fn send(&self, stream: &mut UnixStream, request: Request) -> Result<(), String> {
        let mut line = serde_json::to_string(&Envelope {
            v: VERSION,
            request,
        })
        .map_err(|e| e.to_string())?;
        line.push('\n');
        stream.write_all(line.as_bytes()).map_err(|e| e.to_string())
    }

    /// Sends a request; a response with `ok: false` becomes `Err`.
    pub fn request(&self, request: Request) -> Result<Response, String> {
        let mut stream = self.stream()?;
        self.send(&mut stream, request)?;
        let mut line = String::new();
        BufReader::new(stream)
            .read_line(&mut line)
            .map_err(|e| format!("no answer from the lab: {e}"))?;
        parse(&line)
    }

    /// Like [`Self::request`], also receiving the fds sent along with the response.
    pub fn request_fds(&self, request: Request) -> Result<(Response, Vec<OwnedFd>), String> {
        let mut stream = self.stream()?;
        self.send(&mut stream, request)?;
        let mut buf = vec![0u8; 64 * 1024];
        let mut cmsg = nix::cmsg_space!([RawFd; 8]);
        let (n, fds) = {
            let mut iov = [IoSliceMut::new(&mut buf)];
            let msg = recvmsg::<()>(
                stream.as_raw_fd(),
                &mut iov,
                Some(&mut cmsg),
                MsgFlags::MSG_CMSG_CLOEXEC,
            )
            .map_err(|e| format!("no answer from the lab: {e}"))?;
            let mut fds = Vec::new();
            for c in msg.cmsgs().map_err(|e| e.to_string())? {
                if let ControlMessageOwned::ScmRights(raw) = c {
                    // SAFETY: the kernel just gave us these fds.
                    fds.extend(
                        raw.into_iter()
                            .map(|fd| unsafe { OwnedFd::from_raw_fd(fd) }),
                    );
                }
            }
            (msg.bytes, fds)
        };
        buf.truncate(n);
        if !buf.ends_with(b"\n") {
            stream.read_to_end(&mut buf).map_err(|e| e.to_string())?;
        }
        Ok((parse(&String::from_utf8_lossy(&buf))?, fds))
    }
}

fn parse(line: &str) -> Result<Response, String> {
    let response: Response =
        serde_json::from_str(line.trim()).map_err(|e| format!("bad answer from the lab: {e}"))?;
    if response.ok {
        Ok(response)
    } else {
        Err(response
            .error
            .unwrap_or_else(|| "request failed".to_owned()))
    }
}
