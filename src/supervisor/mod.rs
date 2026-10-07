//! The supervisor (spec §7): owns a lab's namespaces, reconciles changes, runs node processes
//! and serves `ctl.sock`. It stays in the host namespaces for its whole life (spec §2).

mod files;
mod handlers;
mod host;
mod kernel_nat;
mod procs;
mod server;
mod uplink;

pub use host::force_down;
pub use uplink::host_families;

use crate::addr::{Addresses, allocate};
use crate::backend::{self, NetFds, Tools};
use crate::enter::{EnterSpec, NodeSetup};
use crate::labdir::LabDir;
use crate::model::{INTERNET, Topology};
use crate::ns::{self, LabInit};
use crate::plan::Net;
use crate::plan::direct::Ctx;
use crate::plan::{EffEdge, Faults, Target, effective};
use indexmap::IndexMap;
use procs::Proc;
use serde::Serialize;
use std::collections::HashMap;
use std::fs::File;
use std::io::Write;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::{Arc, Condvar, Mutex};

pub struct Options {
    pub file: PathBuf,
    /// Where to report `ok` or `error: …` once the lab is up (for `tpo up -d`).
    pub ready_fd: Option<RawFd>,
    /// Root mode: no user namespace, nodes visible in `/run/netns` (spec §2).
    pub root: bool,
    /// With `root`: the kernel does the internet NAT instead of pasta (spec §6).
    pub kernel_nat: bool,
}

/// A node's (or the gateway's) namespaces.
pub struct NodeNs {
    pub net: OwnedFd,
    pub mnt: OwnedFd,
    pub uts: OwnedFd,
    /// Kept open for the gateway, whose holder must outlive setup (spec §6).
    pub holder: Option<Child>,
    /// The holder's host pid, while it is alive.
    pub holder_pid: Option<i32>,
    /// Root mode: the `/run/netns` name.
    pub netns_name: Option<String>,
}

/// Everything the supervisor knows, behind one lock.
pub struct Lab {
    pub dir: LabDir,
    pub file: PathBuf,
    pub exe: PathBuf,
    pub topo: Topology,
    pub addrs: Addresses,
    pub faults: Faults,
    pub eff: IndexMap<crate::model::Edge, EffEdge>,
    pub desired: Net,
    pub init: LabInit,
    pub nodes: IndexMap<Target, NodeNs>,
    pub tools: Tools,
    pub procs: IndexMap<String, Proc>,
    /// Woken when faults with deadlines change, or on shutdown.
    pub wake: Arc<Condvar>,
    pub shutting_down: bool,
    /// pasta, while the lab uses `internet`.
    pub pasta_pid: Option<i32>,
    /// Nodes currently in the gateway's nft sets.
    pub allowed: uplink::Allowed,
    /// Root mode: what this lab changed on the host.
    pub host: host::HostState,
    /// Use `--kernel-nat` when the lab uses `internet`.
    pub kernel_nat: bool,
    /// Until the lab is up (a signal then means "interrupted", exit 130).
    pub starting: bool,
}

pub type Shared = Arc<Mutex<Lab>>;

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// Runs a supervisor in this process; never returns.
pub fn run(opts: Options) -> ! {
    // The ready pipe must not leak into the lab's processes, or `tpo up -d` never sees EOF.
    if let Some(fd) = opts.ready_fd {
        use nix::fcntl::{FcntlArg, FdFlag, fcntl};
        let _ = fcntl(
            unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) },
            FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC),
        );
    }
    let mut ready = opts
        .ready_fd
        .map(|fd| unsafe { <File as std::os::fd::FromRawFd>::from_raw_fd(fd) });
    match start(&opts) {
        Ok(lab) => {
            if let Some(r) = ready.as_mut() {
                let _ = writeln!(r, "ok");
            } else {
                let l = lab.lock().unwrap();
                println!(
                    "lab `{}` is up ({} nodes); Ctrl-C stops it",
                    l.topo.name,
                    l.topo.nodes.len()
                );
            }
            drop(ready);
            server::serve(lab)
        }
        Err(e) => {
            let e = match crate::doctor::hint_for(&e) {
                Some(hint) => format!("{e}\n  fix: {hint}"),
                None => e,
            };
            match ready.as_mut() {
                Some(r) => {
                    let _ = writeln!(r, "error: {e}");
                }
                None => eprintln!("tpo: {e}"),
            }
            std::process::exit(1);
        }
    }
}

fn start(opts: &Options) -> Result<Shared, String> {
    let source = std::fs::read_to_string(&opts.file)
        .map_err(|e| format!("can't read {}: {e}", opts.file.display()))?;
    let stem = opts
        .file
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let topo =
        Topology::parse(&source, &stem).map_err(|e| format!("{}: {e}", opts.file.display()))?;
    let addrs = allocate(&topo, None).map_err(|e| format!("{}: {e}", opts.file.display()))?;
    let tools = Tools::find()?;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let file = std::fs::canonicalize(&opts.file).unwrap_or_else(|_| opts.file.clone());

    if opts.root && !nix::unistd::geteuid().is_root() {
        return Err("`--root` needs root: run `sudo tpo up --root …`".to_owned());
    }
    if opts.kernel_nat && !opts.root {
        return Err("`--kernel-nat` only works with `--root`".to_owned());
    }
    let dir = LabDir::new(&topo.name);
    if dir.live_pid().is_some() {
        return Err(format!("lab `{}` is already running", topo.name));
    }
    if dir.exists() && host::load(&dir).is_some_and(|h| h.root) {
        return Err(format!(
            "lab `{}` has leftovers from a supervisor that died; clean them up with `sudo tpo down --force --lab {}`",
            topo.name, topo.name
        ));
    }
    // sun_path is 108 bytes including the terminating NUL.
    if dir.ctl().as_os_str().len() > 107 {
        return Err(format!(
            "the control socket path {} is too long for a Unix socket; set XDG_RUNTIME_DIR to a shorter directory",
            dir.ctl().display()
        ));
    }
    dir.remove().map_err(|e| e.to_string())?;
    for d in [dir.path.clone(), dir.logs(), dir.etc()] {
        std::fs::create_dir_all(&d).map_err(|e| format!("can't create {}: {e}", d.display()))?;
    }
    // Outside the lab dir, so it survives teardown.
    redirect_stderr(&dir.supervisor_log(), opts.ready_fd.is_some());

    // Still single-threaded: clone the lab init before anything starts a thread.
    let init = ns::spawn_init(&exe, !opts.root)
        .map_err(|e| format!("can't create the lab namespaces: {e}"))?;
    block_shutdown_signals();
    let slot = server::LabSlot::default();
    server::spawn_signal_thread(slot.clone(), init.pid, dir.clone(), opts.root);

    let topo_mode = topo.mode;
    let lab = Lab {
        dir,
        file,
        exe,
        topo,
        addrs,
        faults: Faults::default(),
        eff: IndexMap::new(),
        desired: Net::empty(topo_mode),
        init,
        nodes: IndexMap::new(),
        tools,
        procs: IndexMap::new(),
        wake: Arc::new(Condvar::new()),
        shutting_down: false,
        pasta_pid: None,
        allowed: uplink::Allowed::new(),
        host: host::HostState {
            root: opts.root,
            ..Default::default()
        },
        kernel_nat: opts.kernel_nat,
        starting: true,
    };
    files::write_etc(&lab.dir, &lab.topo, &lab.addrs, &lab.resolv_conf())
        .map_err(|e| format!("can't write the lab's /etc files: {e}"))?;
    let shared = Arc::new(Mutex::new(lab));
    *slot.lock().unwrap() = Some(shared.clone());
    {
        let mut lab = shared.lock().unwrap();
        let result = build(&mut lab);
        if let Err(e) = result {
            lab.teardown();
            return Err(e);
        }
        lab.save();
    }
    procs::start_all(&shared);
    std::fs::write(
        shared.lock().unwrap().dir.pid_file(),
        format!("{}\n", std::process::id()),
    )
    .map_err(|e| e.to_string())?;
    shared.lock().unwrap().starting = false;
    server::spawn_timer_thread(shared.clone());
    Ok(shared)
}

/// After `tpo up -d` detaches, our stderr goes to the lab's log.
fn redirect_stderr(log: &Path, detached: bool) {
    if !detached {
        return;
    }
    if let Ok(f) = File::options().create(true).append(true).open(log) {
        let _ = nix::unistd::dup2_stderr(&f);
    }
}

fn block_shutdown_signals() {
    use nix::sys::signal::{SigSet, SigmaskHow, Signal, pthread_sigmask};
    let mut set = SigSet::empty();
    for s in [Signal::SIGINT, Signal::SIGTERM, Signal::SIGHUP] {
        set.add(s);
    }
    let _ = pthread_sigmask(SigmaskHow::SIG_BLOCK, Some(&set), None);
}

/// Creates every endpoint's namespaces and applies the initial plan.
fn build(lab: &mut Lab) -> Result<(), String> {
    let mut endpoints: Vec<(Target, String)> = lab
        .topo
        .nodes
        .keys()
        .map(|n| (Target::node(n), n.clone()))
        .collect();
    if lab.topo.uses_internet() {
        endpoints.push((Target::Gateway, "gateway".to_owned()));
    }
    for (target, hostname) in endpoints {
        let keep = target == Target::Gateway;
        let ns = spawn_holder(lab, &hostname, keep)?;
        lab.host.netns.extend(ns.netns_name.clone());
        lab.nodes.insert(target, ns);
        lab.save();
    }
    lab.eff = effective(&lab.topo, &lab.faults, now_ms());
    let ctx = Ctx::new(&lab.topo, &lab.addrs);
    let desired = Net::plan(&ctx, &lab.eff, &Net::empty(lab.topo.mode));
    let ops = desired.initial_ops();
    backend::run(&lab.netfds(), &lab.tools, &ops)?;
    lab.desired = desired;
    if lab.topo.uses_internet() {
        lab.start_uplink()?;
    }
    Ok(())
}

/// Starts a namespace holder in the lab's user and pid namespaces and opens its namespaces.
pub(super) fn spawn_holder(lab: &Lab, hostname: &str, keep: bool) -> Result<NodeNs, String> {
    let etc = lab.dir.etc();
    let mut binds = vec![
        (etc.join("hosts"), PathBuf::from("/etc/hosts")),
        (etc.join("resolv.conf"), PathBuf::from("/etc/resolv.conf")),
    ];
    if Path::new("/etc/nsswitch.conf").exists() {
        binds.push((
            etc.join("nsswitch.conf"),
            PathBuf::from("/etc/nsswitch.conf"),
        ));
    }
    let spec = EnterSpec {
        user: lab.init.user.as_ref().map(AsRawFd::as_raw_fd),
        pid: Some(lab.init.pid_ns.as_raw_fd()),
        setup: Some(NodeSetup {
            hostname: hostname.to_owned(),
            binds,
        }),
        hold: true,
        ..EnterSpec::default()
    };
    let (mut child, host_pid) = lab
        .spawn_enter(spec, Stdio::piped(), Stdio::null(), Stdio::inherit())
        .map_err(|e| format!("node `{hostname}` {e}"))?;
    let open = |kind| {
        ns::open_ns(host_pid, kind)
            .map_err(|e| format!("can't open `{hostname}`'s {kind} namespace: {e}"))
    };
    let netns_name = if lab.host.root {
        let name = host::netns_name(&lab.topo.name, hostname);
        host::attach_netns(&name, host_pid)?;
        Some(name)
    } else {
        None
    };
    let node = NodeNs {
        net: open("net")?,
        mnt: open("mnt")?,
        uts: open("uts")?,
        holder: None,
        holder_pid: None,
        netns_name,
    };
    if keep {
        Ok(NodeNs {
            holder: Some(child),
            holder_pid: Some(host_pid),
            ..node
        })
    } else {
        drop(child.stdin.take());
        let _ = child.wait();
        Ok(node)
    }
}

#[derive(Serialize)]
struct SavedState<'a> {
    supervisor_pid: u32,
    init_pid: i32,
    file: &'a Path,
    topology: &'a Topology,
    addresses: &'a Addresses,
    faults: &'a Faults,
    pasta_pid: Option<i32>,
    host: &'a host::HostState,
}

impl Lab {
    pub fn netfds(&self) -> NetFds {
        let mut nets = HashMap::new();
        nets.insert(
            Target::Hub,
            self.init.net.try_clone().expect("dup hub netns fd"),
        );
        for (t, n) in &self.nodes {
            nets.insert(t.clone(), n.net.try_clone().expect("dup netns fd"));
        }
        NetFds {
            user: self
                .init
                .user
                .as_ref()
                .map(|u| u.try_clone().expect("dup userns fd")),
            hub_pid: self.init.pid.as_raw(),
            nets,
        }
    }

    /// Spawns `tpo __enter` (see [`crate::enter::spawn`]).
    pub fn spawn_enter(
        &self,
        spec: EnterSpec,
        stdin: Stdio,
        stdout: Stdio,
        stderr: Stdio,
    ) -> Result<(Child, i32), String> {
        crate::enter::spawn(&self.exe, spec, stdin, stdout, stderr)
            .map_err(|e| format!("{e} (see {})", self.dir.supervisor_log().display()))
    }

    pub fn save(&self) {
        if let Err(e) = self.try_save() {
            eprintln!("tpo: can't save {}: {e}", self.dir.state().display());
        }
    }

    fn try_save(&self) -> Result<(), String> {
        let state = SavedState {
            supervisor_pid: std::process::id(),
            init_pid: self.init.pid.as_raw(),
            file: &self.file,
            topology: &self.topo,
            addresses: &self.addrs,
            faults: &self.faults,
            pasta_pid: self.pasta_pid,
            host: &self.host,
        };
        let json = serde_json::to_string_pretty(&state).map_err(|e| e.to_string())?;
        let tmp = self.dir.state().with_extension("json.tmp");
        std::fs::write(&tmp, json).map_err(|e| e.to_string())?;
        std::fs::rename(tmp, self.dir.state()).map_err(|e| e.to_string())
    }

    /// Kills every lab process (the pid namespace dies with its init) and removes the lab dir.
    pub fn teardown(&mut self) {
        self.shutting_down = true;
        self.stop_uplink();
        let _ = nix::sys::signal::kill(self.init.pid, nix::sys::signal::Signal::SIGKILL);
        for ns in self.nodes.values_mut() {
            if let Some(mut h) = ns.holder.take() {
                let _ = h.kill();
                let _ = h.wait();
            }
        }
        for name in std::mem::take(&mut self.host.netns) {
            host::delete_netns(&name);
        }
        let _ = self.dir.remove();
    }

    /// Deletes a removed endpoint's `/run/netns` entry (root mode).
    pub fn forget_netns(&mut self, name: Option<&str>) {
        if let Some(name) = name {
            host::delete_netns(name);
            self.host.netns.retain(|n| n != name);
        }
    }

    /// The lab's `/etc/resolv.conf`: the DNS-forward addresses the uplink can serve.
    pub fn resolv_conf(&self) -> String {
        if !self.topo.uses_internet() {
            return String::new();
        }
        if !self.kernel_nat {
            return uplink::resolv_conf();
        }
        let k = kernel_nat::KernelNat::new(crate::addr::lab_hash(&self.topo.name));
        let (dns4, dns6) =
            kernel_nat::resolvers(&std::fs::read_to_string("/etc/resolv.conf").unwrap_or_default());
        let mut out = String::new();
        if dns4.is_some() {
            out.push_str(&format!("nameserver {}\n", uplink::DNS4));
        }
        if k.forwards_dns6(dns6) {
            out.push_str(&format!("nameserver {}\n", uplink::DNS6));
        }
        out
    }

    /// Whether the internet uplink is up (pasta or kernel NAT).
    pub fn uplink_active(&self) -> bool {
        self.pasta_pid.is_some() || self.host.kernel_nat.is_some()
    }

    /// Installs the gateway's nft ruleset and starts the uplink (spec §6).
    pub fn start_uplink(&mut self) -> Result<(), String> {
        let holder = self
            .nodes
            .get(&Target::Gateway)
            .and_then(|g| g.holder_pid)
            .ok_or("the gateway namespace is missing")?;
        let allowed = self.desired.internet_allowed();
        if !self.kernel_nat {
            let ruleset = crate::plan::Op::nft(&Target::Gateway, uplink::ruleset(&allowed));
            backend::run(&self.netfds(), &self.tools, &[ruleset])?;
            self.allowed = allowed;
            self.pasta_pid = Some(uplink::start_pasta(self, holder)?);
            return Ok(());
        }

        let k = kernel_nat::KernelNat::new(crate::addr::lab_hash(&self.topo.name));
        let (dns4, dns6) =
            kernel_nat::resolvers(&std::fs::read_to_string("/etc/resolv.conf").unwrap_or_default());
        let route_localnet =
            dns4.is_some_and(|d| d.is_loopback()) || self.topo.internet.host_loopback;
        let mut ops = vec![crate::plan::Op::nft(
            &Target::Gateway,
            uplink::gateway_ruleset(&allowed, kernel_nat::GW_IF, false),
        )];
        ops.extend(k.setup_ops(&self.topo, &self.addrs, route_localnet));
        ops.push(crate::plan::Op::nft(
            &Target::Host,
            k.host_ruleset(&self.topo, dns4, dns6),
        ));
        // Record before applying, so a crash part-way through can still be cleaned up.
        self.host.kernel_nat = Some(k);
        self.save();
        backend::run(&self.netfds(), &self.tools, &ops)?;
        self.host.sysctls = host::Sysctls::host().enable_forwarding()?;
        self.allowed = allowed;
        self.save();
        Ok(())
    }

    pub fn stop_uplink(&mut self) {
        if let Some(pid) = self.pasta_pid.take() {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(pid),
                nix::sys::signal::Signal::SIGTERM,
            );
        }
        if let Some(k) = self.host.kernel_nat.take() {
            for op in k.teardown_ops() {
                if let Err(e) = backend::run(&self.netfds(), &self.tools, &[op]) {
                    eprintln!("tpo: {e}");
                }
            }
            if !host::other_kernel_nat_lab(&self.topo.name) {
                host::Sysctls::host().restore(&std::mem::take(&mut self.host.sysctls));
            }
        }
        self.allowed.clear();
    }

    pub fn endpoint_env(&self, node: &str) -> Vec<(String, String)> {
        let var = |n: &str| n.to_uppercase().replace('-', "_");
        let mut env = vec![
            ("TPO_LAB".to_owned(), self.topo.name.clone()),
            ("TPO_NODE".to_owned(), node.to_owned()),
        ];
        if let Some(a) = self.addrs.nodes.get(node) {
            env.push(("TPO_ADDR".to_owned(), a.v4.to_string()));
            env.push(("TPO_ADDR6".to_owned(), a.v6.to_string()));
        }
        for (n, a) in &self.addrs.nodes {
            env.push((format!("TPO_ADDR_{}", var(n)), a.v4.to_string()));
            env.push((format!("TPO_ADDR6_{}", var(n)), a.v6.to_string()));
        }
        env
    }

    pub fn node_ns(&self, node: &str) -> Result<&NodeNs, String> {
        let target = if node == INTERNET {
            Target::Gateway
        } else {
            Target::node(node)
        };
        self.nodes
            .get(&target)
            .ok_or_else(|| format!("unknown node `{node}`"))
    }
}
