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

fn strings(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| (*s).to_owned()).collect()
}

fn ms(d: Option<Duration>) -> Option<u64> {
    d.map(|d| d.as_millis() as u64)
}

/// Typed requests, mirroring the CLI. Mutating requests return the effective changes (or, for
/// process control, one status line per node).
impl Client {
    /// Nodes, processes and links, as `tpo show --json` prints them.
    pub fn show(&self) -> Result<serde_json::Value, String> {
        Ok(self.request(Request::Show)?.data)
    }

    pub fn cut(
        &self,
        sides: &[&str],
        oneway: bool,
        for_: Option<Duration>,
    ) -> Result<Vec<String>, String> {
        Ok(self
            .request(Request::Cut {
                sides: strings(sides),
                oneway,
                for_ms: ms(for_),
            })?
            .changes)
    }

    /// `assignments` are `key=value` (or `key=` to drop an override).
    pub fn set(
        &self,
        sides: &[&str],
        assignments: &[&str],
        oneway: bool,
        for_: Option<Duration>,
    ) -> Result<Vec<String>, String> {
        let request = Request::Set {
            sides: strings(sides),
            assignments: strings(assignments),
            oneway,
            for_ms: ms(for_),
        };
        Ok(self.request(request)?.changes)
    }

    pub fn heal(&self, sides: &[&str]) -> Result<Vec<String>, String> {
        Ok(self
            .request(Request::Heal {
                sides: strings(sides),
                all: false,
            })?
            .changes)
    }

    pub fn heal_all(&self) -> Result<Vec<String>, String> {
        Ok(self
            .request(Request::Heal {
                sides: vec![],
                all: true,
            })?
            .changes)
    }

    pub fn apply(&self, path: &std::path::Path) -> Result<Vec<String>, String> {
        let source = std::fs::read_to_string(path)
            .map_err(|e| format!("can't read {}: {e}", path.display()))?;
        Ok(self
            .request(Request::Apply {
                path: path.display().to_string(),
                source,
            })?
            .changes)
    }

    pub fn stop(&self, selector: &str, signal: Option<&str>) -> Result<Vec<String>, String> {
        let request = Request::Stop {
            selector: selector.to_owned(),
            signal: signal.map(str::to_owned),
            grace_ms: None,
            for_ms: None,
        };
        Ok(self.request(request)?.changes)
    }

    pub fn start(&self, selector: &str) -> Result<Vec<String>, String> {
        Ok(self
            .request(Request::Start {
                selector: selector.to_owned(),
            })?
            .changes)
    }

    pub fn restart(&self, selector: &str) -> Result<Vec<String>, String> {
        Ok(self
            .request(Request::Restart {
                selector: selector.to_owned(),
            })?
            .changes)
    }

    pub fn down(&self) -> Result<(), String> {
        self.request(Request::Down).map(|_| ())
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
