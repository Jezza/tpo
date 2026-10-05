use clap::{Parser, Subcommand};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, IntoRawFd};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, ExitCode, Stdio};
use std::time::Duration;
use tpo::client::Client;
use tpo::enter::{EnterSpec, enter};
use tpo::labdir::{self, LabDir};
use tpo::model::Topology;
use tpo::proto::{Request, Response};
use tpo::units::parse_duration;

#[derive(Parser)]
#[command(
    name = "tpo",
    version,
    about = "Virtual network topologies from namespaces and tc, changeable live"
)]
struct Cli {
    /// Which running lab to talk to (default: the only one).
    #[arg(long, global = true)]
    lab: Option<String>,
    /// Machine-readable output.
    #[arg(long, global = true)]
    json: bool,
    /// How long to wait for the lab to answer.
    #[arg(long, global = true, default_value = "10s", value_parser = parse_dur)]
    timeout: Duration,
    #[command(subcommand)]
    command: Cmd,
}

fn parse_dur(s: &str) -> Result<Duration, String> {
    parse_duration(s)
}

#[derive(Subcommand)]
enum Cmd {
    /// Start a lab from a topology file.
    Up {
        file: PathBuf,
        /// Detach and return once the lab is up.
        #[arg(short, long)]
        detach: bool,
        /// Run as root without a user namespace; nodes appear in `ip netns`.
        #[arg(long)]
        root: bool,
        /// With --root: NAT internet traffic in the host kernel instead of pasta.
        #[arg(long, requires = "root")]
        kernel_nat: bool,
    },
    /// Stop a lab.
    Down {
        /// Clean up after a root-mode lab whose supervisor died.
        #[arg(long)]
        force: bool,
    },
    /// Replace the running lab's topology with a new file.
    Apply { file: PathBuf },
    /// Cut every link between different sides (one side: that side against everyone else).
    Cut {
        #[arg(required = true)]
        sides: Vec<String>,
        #[arg(long)]
        oneway: bool,
        /// Heal automatically after this long.
        #[arg(long = "for", value_parser = parse_dur)]
        for_: Option<Duration>,
    },
    /// Override link properties between sides: `tpo set a b delay=200ms loss=5%`.
    Set {
        #[arg(required = true)]
        args: Vec<String>,
        #[arg(long)]
        oneway: bool,
        #[arg(long = "for", value_parser = parse_dur)]
        for_: Option<Duration>,
    },
    /// Remove faults between sides, or all of them.
    Heal {
        sides: Vec<String>,
        #[arg(long, conflicts_with = "sides")]
        all: bool,
    },
    /// Stop node processes (the network stays up).
    Stop {
        selector: String,
        #[arg(long)]
        signal: Option<String>,
        #[arg(long, value_parser = parse_dur)]
        grace: Option<Duration>,
        /// Start again after this long.
        #[arg(long = "for", value_parser = parse_dur)]
        for_: Option<Duration>,
    },
    /// Start node processes.
    Start { selector: String },
    /// Restart node processes.
    Restart { selector: String },
    /// Run a command inside a node.
    Exec {
        node: String,
        #[arg(last = true, required = true)]
        cmd: Vec<String>,
    },
    /// Open a shell inside a node.
    Shell { node: String },
    /// Show nodes, processes and links.
    Show,
    /// Print the ip/tc state tpo expects for a node.
    Explain { node: String },
    /// Rebuild nodes' network state from scratch.
    Resync { selector: Option<String> },
    /// Print a node's process log.
    Logs {
        node: String,
        #[arg(short, long)]
        follow: bool,
    },
    /// Check that this machine can run labs.
    Doctor,
    #[command(hide = true, name = "__supervise")]
    Supervise {
        file: PathBuf,
        #[arg(long)]
        ready_fd: Option<i32>,
        #[arg(long)]
        root: bool,
        #[arg(long)]
        kernel_nat: bool,
    },
    #[command(hide = true, name = "__init")]
    Init { fd: i32 },
    #[command(hide = true, name = "__enter")]
    Enter { spec: String },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("tpo: {e}");
            ExitCode::FAILURE
        }
    }
}

fn ms(d: Option<Duration>) -> Option<u64> {
    d.map(|d| d.as_millis() as u64)
}

fn run(cli: Cli) -> Result<ExitCode, String> {
    let client = || Client::connect(cli.lab.as_deref()).map(|c| c.with_timeout(cli.timeout));
    let request = match cli.command {
        Cmd::Init { fd } => tpo::ns::run_init(fd),
        Cmd::Enter { spec } => {
            let spec: EnterSpec =
                serde_json::from_str(&spec).map_err(|e| format!("bad spec: {e}"))?;
            enter(&spec)
        }
        Cmd::Supervise {
            file,
            ready_fd,
            root,
            kernel_nat,
        } => tpo::supervisor::run(tpo::supervisor::Options {
            file,
            ready_fd,
            root,
            kernel_nat,
        }),
        Cmd::Up {
            file,
            detach: false,
            root,
            kernel_nat,
        } => tpo::supervisor::run(tpo::supervisor::Options {
            file,
            ready_fd: None,
            root,
            kernel_nat,
        }),
        Cmd::Up {
            file,
            detach: true,
            root,
            kernel_nat,
        } => return up_detached(&file, root, kernel_nat),
        Cmd::Down { force: false } => return down(cli.lab.as_deref()),
        Cmd::Down { force: true } => return down_force(cli.lab.as_deref()),
        Cmd::Doctor => return Ok(doctor(cli.json)),
        Cmd::Exec { node, cmd } => exec(&client()?, &node, cmd),
        Cmd::Shell { node } => {
            let shell = std::env::var("SHELL")
                .ok()
                .filter(|s| std::path::Path::new(s).exists());
            exec(
                &client()?,
                &node,
                vec![shell.unwrap_or_else(|| "/bin/sh".to_owned())],
            )
        }
        Cmd::Logs { node, follow } => return logs(cli.lab.as_deref(), &node, follow),
        Cmd::Show => Request::Show,
        Cmd::Explain { node } => Request::Explain { node },
        Cmd::Cut {
            sides,
            oneway,
            for_,
        } => Request::Cut {
            sides,
            oneway,
            for_ms: ms(for_),
        },
        Cmd::Set { args, oneway, for_ } => {
            let (assignments, sides) = args.into_iter().partition(|a| a.contains('='));
            Request::Set {
                sides,
                assignments,
                oneway,
                for_ms: ms(for_),
            }
        }
        Cmd::Heal { sides, all } => {
            if sides.is_empty() && !all {
                return Err("give sides to heal, or --all".to_owned());
            }
            Request::Heal { sides, all }
        }
        Cmd::Apply { file } => {
            let source = std::fs::read_to_string(&file)
                .map_err(|e| format!("can't read {}: {e}", file.display()))?;
            Request::Apply {
                path: file.display().to_string(),
                source,
            }
        }
        Cmd::Stop {
            selector,
            signal,
            grace,
            for_,
        } => Request::Stop {
            selector,
            signal,
            grace_ms: ms(grace),
            for_ms: ms(for_),
        },
        Cmd::Start { selector } => Request::Start { selector },
        Cmd::Restart { selector } => Request::Restart { selector },
        Cmd::Resync { selector } => Request::Resync { selector },
    };
    let show = matches!(request, Request::Show);
    let explain = matches!(request, Request::Explain { .. });
    let response = client()?.request(request)?;
    if cli.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&response).unwrap_or_default()
        );
    } else if show {
        print!("{}", tpo::render::show(&response.data));
    } else if explain {
        for line in response.data["ops"].as_array().into_iter().flatten() {
            println!("{}", line.as_str().unwrap_or_default());
        }
    } else {
        print_changes(&response);
    }
    Ok(ExitCode::SUCCESS)
}

fn print_changes(response: &Response) {
    if response.changes.is_empty() {
        println!("no changes");
    }
    for line in &response.changes {
        println!("{line}");
    }
}

fn up_detached(file: &PathBuf, root: bool, kernel_nat: bool) -> Result<ExitCode, String> {
    let source =
        std::fs::read_to_string(file).map_err(|e| format!("can't read {}: {e}", file.display()))?;
    let stem = file
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let topo = Topology::parse(&source, &stem).map_err(|e| format!("{}: {e}", file.display()))?;
    if LabDir::new(&topo.name).live_pid().is_some() {
        return Err(format!("lab `{}` is already running", topo.name));
    }
    let (read, write) = nix::unistd::pipe().map_err(|e| e.to_string())?;
    let write_fd = write.as_raw_fd();
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut cmd = Command::new(exe);
    cmd.arg("__supervise")
        .arg(file)
        .arg("--ready-fd")
        .arg(write_fd.to_string())
        .args(root.then_some("--root"))
        .args(kernel_nat.then_some("--kernel-nat"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: setsid and fcntl only.
    unsafe {
        cmd.pre_exec(move || {
            nix::unistd::setsid().map_err(std::io::Error::from)?;
            tpo::ns::inherit(write_fd)
        });
    }
    cmd.spawn()
        .map_err(|e| format!("can't start the supervisor: {e}"))?;
    drop(write);
    let mut answer = String::new();
    let _ = std::io::BufRead::read_line(
        &mut std::io::BufReader::new(std::fs::File::from(read)),
        &mut answer,
    );
    match answer.trim() {
        "ok" => {
            println!("lab `{}` is up ({} nodes)", topo.name, topo.nodes.len());
            Ok(ExitCode::SUCCESS)
        }
        other => Err(other
            .strip_prefix("error: ")
            .map(str::to_owned)
            .unwrap_or_else(|| "the supervisor exited before the lab was up".to_owned())),
    }
}

/// Cleans up after a root-mode lab whose supervisor is gone (or stops it, if it's alive).
fn down_force(lab: Option<&str>) -> Result<ExitCode, String> {
    if !nix::unistd::geteuid().is_root() {
        return Err("`down --force` needs root: run `sudo tpo down --force`".to_owned());
    }
    let dir = match lab {
        Some(name) => LabDir::new(name),
        None => {
            let stale = labdir::stale();
            match stale.as_slice() {
                [one] => one.clone(),
                [] => return down(None),
                many => {
                    let names: Vec<_> = many.iter().map(|l| l.name.as_str()).collect();
                    return Err(format!(
                        "several labs have leftovers ({}); pick one with --lab",
                        names.join(", ")
                    ));
                }
            }
        }
    };
    if dir.live_pid().is_some() {
        return down(Some(&dir.name));
    }
    if !dir.exists() {
        return Err(format!("lab `{}` has nothing to clean up", dir.name));
    }
    for line in tpo::supervisor::force_down(&dir)? {
        println!("{line}");
    }
    Ok(ExitCode::SUCCESS)
}

fn down(lab: Option<&str>) -> Result<ExitCode, String> {
    let client = Client::connect(lab)?;
    let pid = client.lab.live_pid();
    client.request(Request::Down)?;
    if let Some(pid) = pid {
        for _ in 0..100 {
            if !std::path::Path::new(&format!("/proc/{pid}")).exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    println!("lab `{}` is down", client.lab.name);
    Ok(ExitCode::SUCCESS)
}

fn exec(client: &Client, node: &str, cmd: Vec<String>) -> ! {
    let (response, fds) = match client.request_fds(Request::Enter {
        node: node.to_owned(),
    }) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("tpo: {e}");
            std::process::exit(1);
        }
    };
    let names: Vec<String> =
        serde_json::from_value(response.data["fds"].clone()).unwrap_or_default();
    if names.len() != fds.len() {
        eprintln!(
            "tpo: the lab sent {} namespace fds for {} names",
            fds.len(),
            names.len()
        );
        std::process::exit(1);
    }
    let fds: std::collections::HashMap<String, i32> = names
        .into_iter()
        .zip(fds.into_iter().map(IntoRawFd::into_raw_fd))
        .collect();
    let env: Vec<(String, String)> =
        serde_json::from_value(response.data["env"].clone()).unwrap_or_default();
    let spec = EnterSpec {
        user: fds.get("user").copied(),
        mnt: fds.get("mnt").copied(),
        net: fds.get("net").copied(),
        uts: fds.get("uts").copied(),
        pid: fds.get("pid").copied(),
        interactive: nix::unistd::isatty(std::io::stdin()).unwrap_or(false),
        env,
        cmd,
        ..EnterSpec::default()
    };
    enter(&spec)
}

fn logs(lab: Option<&str>, node: &str, follow: bool) -> Result<ExitCode, String> {
    let lab = labdir::find(lab)?;
    let path = lab.log(node);
    let mut f = std::fs::File::open(&path).map_err(|_| format!("no log for `{node}`"))?;
    let mut out = std::io::stdout();
    loop {
        let mut buf = Vec::new();
        f.read_to_end(&mut buf).map_err(|e| e.to_string())?;
        out.write_all(&buf).map_err(|e| e.to_string())?;
        out.flush().ok();
        if !follow || !lab.exists() {
            return Ok(ExitCode::SUCCESS);
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn doctor(json: bool) -> ExitCode {
    let checks = tpo::doctor::run();
    let failed = checks.iter().any(|c| c.required && c.result.is_err());
    if json {
        let v: Vec<_> = checks
            .iter()
            .map(|c| {
                serde_json::json!({
                    "check": c.name,
                    "ok": c.result.is_ok(),
                    "required": c.required,
                    "detail": match &c.result { Ok(d) | Err(d) => d },
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
    } else {
        for c in &checks {
            match &c.result {
                Ok(d) => println!("✓ {}: {d}", c.name),
                Err(d) if c.required => println!("✗ {}: {d}", c.name),
                Err(d) => println!("- {}: {d}", c.name),
            }
        }
        println!();
        println!(
            "{}",
            if failed {
                "this machine can't run labs yet (see ✗ above)"
            } else {
                "ready to run labs"
            }
        );
    }
    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}
