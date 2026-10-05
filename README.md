# tpo

Virtual network topologies on one Linux machine, built from namespaces and `tc`, and changeable
while they run. Describe nodes and links in TOML, bring the lab up without root, run anything
inside the nodes, then cut, degrade and heal links live.

```toml
# chain.toml
[nodes]
a = {}
b = {}
c = {}

[[link]]
path = ["a", "b", "c"]
delay = "20ms"
```

```console
$ tpo up -d chain.toml
lab `chain` is up (3 nodes)
$ tpo exec a -- ping -c1 b          # ≈40 ms: 20 ms each way
$ tpo exec a -- ping -c1 c          # nothing: a and c aren't linked
$ tpo cut a b --for 30s             # partition, healed automatically after 30 s
cut a⇄b
$ tpo set b c delay=200ms loss=5%
b⇄c delay 20ms→200ms
b⇄c loss none→5%
$ tpo heal --all
$ tpo down
```

## Install

```console
$ cargo install tpo
$ tpo doctor
```

Linux only. Needs iproute2 (`ip`, `tc`). Labs that use `internet` also need
[pasta](https://passt.top) (Debian/Ubuntu: `apt install passt`) and nftables.

`tpo doctor` checks everything a lab needs and says how to fix what's missing. Rootless labs
need unprivileged user namespaces. On Ubuntu 23.10 and later, AppArmor restricts them by
default; `doctor` prints the exact `sysctl` to change.

## Topology files

```toml
name = "lab"              # default: the file name
mode = "direct"           # or "routed" (see below)
subnet = "10.77.0.0/24"   # IPv4 node addresses (default shown)
subnet6 = "fd77::/64"     # IPv6 node addresses (default shown)

[defaults]                # applied to every link
delay = "1ms"

[nodes]
a = {}
b = { addr = "10.77.0.50", addr6 = "fd77::50" }
c = {}
d = {}

[groups]
edge = ["c", "d"]

[[link]]                  # a line: a—b, b—c
path = ["a", "b", "c"]

[[link]]                  # a star (or bipartite, with groups on both sides)
from = "b"
to = "edge"
delay = "80ms"
jitter = "10ms"
loss = "2%"

[[link]]                  # everyone in the group to everyone else in it
mesh = "edge"

[[link]]                  # one direction only, refining the link above
from = "a"
to = "b"
oneway = true
rate = "1mbit"

[[link]]                  # internet access (see below)
from = ["a", "b"]
to = "internet"

[internet]
host_loopback = true      # this machine's 127.0.0.1 is reachable from nodes as `host`

[run]                     # what to run in each node: the most specific entry wins
"*" = "my-daemon --name $TPO_NODE"
"edge" = "iperf3 -s"
"c" = ""                  # run nothing on c
```

- **Endpoints**: `path`, `from`, `to` and `mesh` take node names, group names or lists of
  either.
- **Direction**: links are bidirectional unless `oneway = true`. Every property applies per
  direction, so `delay = "20ms"` adds 20 ms each way.
- **Merging**: properties merge in file order, `[defaults]` first. Later links override earlier
  ones per direction and per property.
- **Properties**: `delay`, `jitter`, `loss`, `duplicate`, `reorder`, `corrupt`, `rate`.
- **Addresses**: every node is dual-stack. Its names resolve inside the lab (`/etc/hosts`).
- **`run` commands** get `TPO_LAB`, `TPO_NODE`, `TPO_ADDR`, `TPO_ADDR6`, and
  `TPO_ADDR_<NODE>` / `TPO_ADDR6_<NODE>` for every node. Their output goes to `tpo logs <node>`.
- **`[run]` rules**: a node is matched by its own entry, else its group's, else `*`. A node in
  two groups that both have entries is an error unless it has an entry of its own.

### Modes

- **`direct`**: a link means "these two can talk". All nodes share one segment, and a node
  receives frames only from nodes linked to it, whether IPv4, IPv6, ARP or multicast.
  Nothing forwards packets, so getting from `a` to `c` in a chain is the application's job.
  Use it for mesh, gossip and P2P software, where you control who can *see* whom.
- **`routed`**: a link is a cable between two routers. Nodes forward for each other along the
  shortest path (`metric = "hops"` or `"delay"`), delays add up along the path, and cutting a
  link fails over to another path if there is one. Use it for IP-level and WAN-style testing.

### Internet access

`internet` is a built-in node: link nodes to it like any other node, with any link properties.

- Linked nodes reach the internet over IPv4 and IPv6, by IP and by name.
- They stay isolated from each other, so two nodes linked only to `internet` can meet only
  through something outside the lab, such as a relay or lighthouse.
- `tpo cut a internet` is a WAN outage, including for connections that are already open.

## Commands

| | |
|---|---|
| `tpo up <file> [-d]` | start a lab (foreground, or detached with `-d`) |
| `tpo down` | stop it; nothing is left behind |
| `tpo show` | nodes, processes, links and faults |
| `tpo exec <node> -- <cmd>`, `tpo shell <node>` | run something inside a node |
| `tpo cut <side>…` | cut every link between different sides |
| `tpo set <side>… key=value…` | change link properties (`key=` restores the file's value) |
| `tpo heal <side>…`, `tpo heal --all` | undo cuts and `set`s |
| `tpo apply <file>` | change the topology of the running lab |
| `tpo stop/start/restart <selector>` | control node processes (the network stays up) |
| `tpo logs <node> [-f]` | a node's process output |
| `tpo explain <node>` | the `ip`/`tc` commands that build a node |
| `tpo doctor` | check this machine |
| `sudo tpo up --root [--kernel-nat] <file>` | root mode (see below) |
| `sudo tpo down --force [--lab <name>]` | clean up after a root lab whose supervisor died |

**Sides and selectors.** Each argument to `cut`, `set` and `heal` is a *side*: a node, a group,
`*`, or a comma list. The command affects every link between two different sides.
- `tpo cut a b` cuts a⇄b.
- `tpo cut edge` cuts the group off from everyone else while it stays connected internally.
- `tpo cut d e *` isolates d and e from everyone, including each other.

**Modifiers.**
- `--oneway` applies to the first side → second only.
- `--for <duration>` undoes a fault automatically after that long.
- `--json` gives machine-readable output.

**Faults are separate from the file.** Faults (cuts and `set`s) sit on top of the topology file
and survive `apply`. `tpo heal --all` always returns to exactly what the file says.

## Root mode

`sudo tpo up --root lab.toml` runs the lab without a user namespace:
- Every node also appears in `ip netns` as `tpo-<lab>-<node>`, so other tools can use it, e.g.
  `ip netns exec tpo-lab-a tcpdump -i eth0`.
- Internet access still goes through pasta by default, which changes nothing on the host.

`--kernel-nat` makes the host kernel do the NAT instead, for more throughput and working ping.
It adds a veth and an nft table on the host and turns on forwarding. Everything it changes is
undone on `down`, including the previous sysctl values. A forward chain that drops by default
(Docker, ufw, firewalld) blocks it; `sudo tpo doctor` detects that and shows the fix.

If a root lab's supervisor dies, its namespaces stay behind. `sudo tpo down --force` removes them.

## From Rust

```rust
use tpo::client::Client;

let lab = Client::connect(Some("chain"))?;
lab.cut(&["a", "b"], false, None)?;
lab.set(&["b", "c"], &["delay=200ms"], false, None)?;
lab.heal_all()?;
```

## How it works

- **Supervisor.** `tpo up` starts a supervisor that stays in your normal namespaces. It
  creates a user namespace for the lab, so no root is needed, plus one network, mount and UTS
  namespace per node. Everything in the lab runs in one pid namespace, so stopping the
  supervisor, even with `kill -9`, takes the whole lab with it.
- **Direct mode** uses one bridge. Each node has an HTB classifier with netem per peer, and an
  ingress allowlist by MAC address.
- **Routed mode** uses a veth pair per link and static routes.
- **Internet access** uses [pasta](https://passt.top) attached to a gateway namespace, with
  nftables doing the NAT and the per-node access control.

See [`docs/specs`](docs/specs) for the full design.

## Limitations

- Linux only.
- Bridging nodes to physical interfaces isn't supported yet.
- In direct mode, broadcast and multicast aren't shaped by link properties.
- In routed mode, multicast doesn't cross hops.
- Rootless ICMP (ping) to the internet needs `net.ipv4.ping_group_range` to allow it. TCP and
  UDP always work.

## License

MIT or Apache-2.0, at your option.
