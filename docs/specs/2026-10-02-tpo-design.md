# tpo — design

Date: 2026-10-02
Status: draft for review

## 1. Purpose

`tpo` is a standalone, workload-agnostic CLI (and library) for building virtual network topologies
on a single Linux machine and changing them while they run. You describe nodes and links at a high
level in TOML; `tpo` builds them from network namespaces, veths, bridges and `tc`, runs whatever
commands you want inside each node, and lets you cut, degrade and heal links live.

Motivating use: testing myrmic's behaviour under non-clique topologies, partitions and degraded
links — but nothing in `tpo` knows about myrmic. Any process can run in a node.

Published on crates.io as `tpo` (name verified free 2026-10-02). Linux only.

### Success criteria

- A 5-node chain/star/mesh is a few lines of TOML; `tpo up lab.toml` brings it up rootless.
- Real processes run in nodes; `tpo exec`/`tpo shell` behave like a normal terminal.
- Live changes (`cut`, `set`, `heal`, `apply`, `stop`/`start`) take effect without tearing down
  the lab and without disturbing unrelated links.
- Measured behaviour matches the spec (RTT ≈ configured delay, unreachable pairs really are).
- Nodes can be given internet access while staying isolated from each other.
- Failures are reported with a cause and a fix, never a bare `EPERM`.

### Non-goals (v1)

IPv6; restart policies for node processes; topology generators/templates; netlink backend;
pasta uplink; bridging to physical NICs; ICMP to the internet when rootless.

## 2. Privilege model

**Rootless (default).** The supervisor runs `unshare(CLONE_NEWUSER | CLONE_NEWNET)` mapping the
invoking uid to root, which grants `CAP_NET_ADMIN` over namespaces it owns and nothing else.

Verified on Debian 13 / kernel 7.1.13 (2026-10-02), unprivileged:

- bridge + veth into per-node netns; traffic flows
- `tc` netem, HTB root + per-destination classes, `u32`/`flower` filters, ingress `action drop`,
  `clsact`; qdisc/classifier modules autoload from inside the userns
- per-netns `sysctl net.ipv4.ip_forward`, loopback `/32`s, static routes
- an outside process joining via `setns` on a userns + a netns held only by an fd
  (`/proc/<supervisor>/fd/<n>`)
- slirp4netns uplink + `nft` masquerade in a gateway netns; node reaches internet over TCP
- per-node mount namespace bind-mounting `/etc/resolv.conf` and `/etc/hosts`

Known environments where rootless fails (detected by `tpo doctor`, see §8):

- Ubuntu ≥ 23.10 with `kernel.apparmor_restrict_unprivileged_userns=1` (userns has no caps)
- `kernel.unprivileged_userns_clone=0` or `user.max_user_namespaces=0`
- inside Docker/CI containers (default seccomp blocks `CLONE_NEWUSER`)
- kernels lacking netem/HTB or with module autoload locked down

**Root (`--root`, opt-in).** Same supervisor and protocol, but nodes are real
`ip netns add tpo-<lab>-<node>` namespaces visible to `ip netns exec` and other tools. Namespaces
outlive a crashed supervisor; `tpo down --force` cleans them from `state.json`.

`ip`, `tc`, `nft`, `slirp4netns` are invoked by absolute path (resolved once at startup, searching
`/usr/sbin:/sbin:/usr/bin:/bin` and `$PATH`), since `/usr/sbin` is often not on a user's `PATH`.

## 3. Topology file (TOML)

```toml
name   = "chain"          # default: file stem
mode   = "direct"         # "direct" | "routed"
subnet = "10.77.0.0/24"   # node addresses, assigned in declaration order

[defaults]                # applied to every link before any [[link]]
delay = "1ms"

[nodes]
a = {}
b = {}
c = { addr = "10.77.0.50" }
d = {}
e = {}

[groups]
edge = ["d", "e"]

[[link]]                  # line
path = ["a", "b", "c"]
delay = "20ms"

[[link]]                  # star / bipartite
from = "c"
to = "edge"
delay = "80ms"
jitter = "10ms"
loss = "2%"

[[link]]                  # clique
mesh = "edge"

[[link]]                  # one direction only
from = "a"
to = "b"
oneway = true
rate = "1mbit"

[[link]]                  # internet uplink (see §6)
from = ["a", "b"]
to = "internet"
delay = "30ms"

[internet]
host_loopback = false     # true: host's 127.0.0.1 reachable as 10.0.2.2

[run]
"*"    = "myrmic runtimes start --tmp --name $TPO_NODE"
"edge" = "iperf3 -s"
"c"    = ""               # run nothing on c
```

### 3.1 Names and selectors

- Node and group names share one namespace; a clash is an error. `internet` and `*` are reserved.
- A **selector** is `*`, a node name, a group name, or a comma list of those (`a,edge`).
  The same syntax is used in the file and on the CLI.

### 3.2 Links

- Endpoints (`from`, `to`, `path` elements, `mesh`) accept a node, a group, or a list of either;
  groups expand to their members.
  - `path = [x, y, z]`: links x–y and y–z (each step expands bipartitely).
  - `from`/`to`: every member of `from` to every member of `to`.
  - `mesh`: every pair of distinct members.
- Bidirectional unless `oneway = true` (only `from` → `to`; invalid with `path`/`mesh`).
- Self-links are ignored; a link whose expansion is empty is an error.
- Internally the topology is a set of **directed edges** `(from, to)`, each with its own
  properties.

### 3.3 Link properties

| key         | netem/HTB mapping        | example   |
|-------------|--------------------------|-----------|
| `delay`     | netem `delay`            | `"20ms"`  |
| `jitter`    | netem `delay … <jitter>` | `"5ms"`   |
| `loss`      | netem `loss`             | `"2%"`    |
| `duplicate` | netem `duplicate`        | `"1%"`    |
| `reorder`   | netem `reorder`          | `"10%"`   |
| `corrupt`   | netem `corrupt`          | `"0.1%"`  |
| `rate`      | HTB class `rate`/`ceil`  | `"1mbit"` |

Values are parsed and validated at load. `jitter`/`reorder` without `delay` is an error.

**Merge order:** `[defaults]` → each `[[link]]` in file order. Later entries win **per directed
edge, per property**; unspecified properties are inherited. So a later `oneway` entry refines one
direction of an earlier bidirectional link.

When an edge's effective properties are all zero/unset, no netem is installed for it.

### 3.4 Addresses

- Node addresses come from `subnet` in declaration order, skipping explicit `addr`s; the subnet
  must fit all nodes (+1 for the gateway if `internet` is used). Clashes are errors.
- Routed mode adds `link_subnet` (default `10.78.0.0/16`), carved into a `/31` per link, and
  `metric = "hops" | "delay"` (default `hops`).

### 3.5 `run`

- Keys are selectors; values are commands executed via `sh -c` in the node.
- **Most specific wins:** node entry > group entry > `*` > nothing. Comma lists are split and each
  part counts at its own level (`"a,edge"` = node entry for `a` + group entry for `edge`).
- Errors:
  - a node named by two node-level entries (e.g. keys `"a"` and `"a,b"`)
  - a node with no node-level entry that is in two groups which both have entries
    (resolved by adding a node entry)
  - unknown names
- `""` means run nothing.
- Environment: `TPO_LAB`, `TPO_NODE`, `TPO_ADDR`, and `TPO_ADDR_<NODE>` for every node
  (uppercased, non-alphanumerics → `_`).
- stdout/stderr → `<labdir>/logs/<node>.log`.

## 4. Network modes

### 4.1 `direct` — reachability = adjacency

```
        hidden bridge (one L2 segment, `subnet`)
     ┌──────────┼──────────┐
   a .1       b .2       c .3
```

- A hub namespace holds a bridge; each node has one veth `eth0` onto it.
- **Egress (shaping):** per node, HTB root (`default` → unshaped class) with one class per
  peer that has an edge from this node, a netem child qdisc for its properties, and a `u32`
  filter on destination address. Handles are derived from the peer's stable index
  (`1:<10+idx>`, `<10+idx>:`), so changing one edge never touches another.
- **Ingress (reachability):** per node, `clsact`/ingress with `action drop` filters for every
  source that has no live edge *to* this node. Filtering at the receiver also confines
  broadcast/multicast discovery to neighbours.
- No IP forwarding: a node reaches only nodes it has an edge to. Multi-hop delivery is the
  application's job.

### 4.2 `routed` — reachability = path

```
   a ──veth── b ──veth── c        each link its own /31 from link_subnet
  10.77.0.1  10.77.0.2  10.77.0.3   (node address /32 on lo)
```

- Each link is a veth pair between the two node namespaces; each node's address is a `/32` on
  `lo`; forwarding is enabled in every node.
- Static routes to every node's `/32` along the shortest path (`metric`); latency adds up along
  the path.
- netem/HTB on each veth's egress, per direction.
- Cutting a link takes the veth down and recomputes routes; alternate paths take over.
- Multicast does not cross hops.

## 5. Node environment

Each node is a network namespace **and** a mount namespace (both held by the supervisor via fds).
The mount namespace bind-mounts:

- `/etc/hosts`: every node's name → address (so `curl b:8080` works), plus `internet` gateway
- `/etc/resolv.conf`: `nameserver 10.0.2.3` when the lab uses `internet`; otherwise empty
  (DNS fails fast instead of hanging on the host's resolver)

`tpo exec`/`shell` join the user, network and mount namespaces.

## 6. Internet access

- `internet` is a reserved built-in node, created only if some link references it. It behaves
  like any node for links, faults and properties (`tpo cut a internet` = WAN outage).
- Implementation: a hidden gateway namespace with a slirp4netns uplink (`tap0`, NAT to the host's
  network), `nft` masquerade on `tap0`, and forwarding enabled.
  - direct: the gateway is on the bridge; only nodes with a live edge to `internet` get
    `default via <gateway>`.
  - routed: `internet` is a router node; linked nodes get a default route along the shortest
    path.
- The gateway **drops lab→lab forwarding**: nodes linked only to `internet` can meet only via
  something outside the lab (e.g. a lighthouse).
- Enforcement of the `internet` edge is at the gateway, not by node ingress filters (internet
  traffic carries non-lab source addresses): the gateway's `nft` forward chain accepts only lab
  sources with a live edge to `internet` (plus their established return traffic), and the node's
  default route is added/removed with the edge. Node ingress drop filters match lab addresses
  only.
- `oneway` and `--oneway` are not supported on edges to `internet` in v1 (error).
- netem for the `internet` edge is applied on the node's egress toward the gateway and on the
  gateway's egress toward the node.
- `[internet] host_loopback = true` maps the host's `127.0.0.1` to `10.0.2.2` (slirp4netns
  default, otherwise disabled).
- Rootless ICMP to the internet requires the host's `net.ipv4.ping_group_range` to allow it;
  TCP/UDP always work.
- slirp4netns is a runtime dependency only for labs using `internet`.

## 7. Runtime: supervisor and CLI

One crate (`tpo`, lib + bin). The supervisor is the same binary.

### 7.1 Lifecycle

- `tpo up lab.toml` runs in the foreground (Ctrl-C tears down); `tpo up -d` re-execs as hidden
  `tpo __supervise`, detaches, and returns once the lab is ready.
- The supervisor creates the userns/netns, and for each node forks a short-lived child that
  unshares net+mnt namespaces, sets up its mounts, and exits after the supervisor has opened fds
  on `/proc/<child>/ns/{net,mnt}`.
- Lab directory `$XDG_RUNTIME_DIR/tpo/<lab>/` (root mode: `/run/tpo/<lab>/`):
  `pid`, `ctl.sock` (JSON-lines protocol), `state.json`, `logs/`.
- Node processes (from `run`) are spawned by the supervisor in their own process group with
  `PR_SET_PDEATHSIG`, so a dead supervisor takes them with it.
- `tpo down` kills node processes and exits; closing the fds destroys every namespace.

### 7.2 Protocol

- All mutating commands are thin clients: one request over `ctl.sock`, one reply with the
  effective diff or an error. The supervisor serialises requests; only it runs `ip`/`tc`.
- `exec`/`shell` are the exception: the CLI `setns`es into the node itself (via
  `/proc/<supervisor>/fd/<n>`) and execs, so tty/stdio/signals are native.

### 7.3 State: spec + faults

- **Spec**: the topology from the file. Changed only by `up`/`apply`.
- **Faults**: runtime changes, stored per expanded directed edge (cut flag, property overrides,
  optional expiry). Faults can only cut or modify edges the spec has; they never add edges.
- Faults survive `apply` (and are dropped for edges that no longer exist).
  `tpo heal --all` returns exactly to the spec.

### 7.4 Commands

Sides/selectors use the §3.1 syntax.

| command | effect |
|---|---|
| `tpo up <file> [-d] [--root]` | start a lab |
| `tpo down [--force]` | stop a lab (`--force`: clean up after a dead root-mode supervisor) |
| `tpo apply <file>` | replace the spec: add/remove nodes and links, change properties; a node's process restarts only if its resolved `run` command changed |
| `tpo cut <side> [<side>…] [--oneway] [--for <dur>]` | cut every edge between different sides; edges within a side are untouched; one side = `<side> *` |
| `tpo set <side> <side> k=v… [--oneway] [--for <dur>]` | override properties on existing edges between the sides; `k=` drops the override for `k` |
| `tpo heal <side> [<side>…]` / `tpo heal --all` | remove faults on matching edges |
| `tpo stop <sel> [--signal SIG] [--grace 5s] [--for <dur>]` | stop node processes (SIGTERM → SIGKILL after grace); network untouched |
| `tpo start <sel>` / `tpo restart <sel>` | start/restart node processes |
| `tpo exec <node> -- <cmd…>` / `tpo shell <node>` | run a command / interactive shell in a node |
| `tpo show` | nodes, process states, spec, faults, effective edges |
| `tpo explain <node>` | the `ip`/`tc` state the plan expects for a node |
| `tpo logs <node> [-f]` | node process logs |
| `tpo doctor` | preflight checks (§8) |

Lab selection: `--lab <name>`; defaults to the only running lab, error if several.

Side rules (`cut`/`set`/`heal`):

- Sides must be disjoint; `*` means every node not named in another side (incl. `internet` if
  present).
- `--oneway` requires exactly two sides (first → second).
- No edges between the sides is an error.

Process commands (`stop`/`start`/`restart`) report per node (`already running`, `no command`,
`exited(1)` …) and act in parallel; only unknown names are errors.

### 7.5 Reconciliation

Every request:

1. validate against the model (no side effects on failure)
2. compute the effective directed-edge set (spec ⊕ faults)
3. diff against the previous effective set
4. emit minimal per-edge operations
   - direct: `tc qdisc change` (netem), `tc class change` (rate), add/remove one ingress drop
     filter (cut/heal), add/remove a peer's class+netem+filter (apply)
   - routed: `tc qdisc change` on the veth; veth up/down; recompute shortest paths;
     `ip route replace`/`del` only for changed routes
   - internet: add/remove default route on the node
5. apply, and return the diff (`a→b delay 20ms→200ms`, `cut a⇄b`, `route a→c via b → via d`)

## 8. Errors

- **Validation first**: unknown names, `run` ambiguities, overlapping sides, empty cuts,
  malformed values, address clashes, undersized subnets → precise error, nothing changed.
- **`tpo doctor`** (also run by `up`) probes userns creation, caps inside it, veth/bridge,
  netem/HTB/u32 qdiscs, mount namespace, tool paths, and slirp4netns/nft when `internet` is used.
  Each failure maps to a cause and fix (e.g. "AppArmor restricts unprivileged user namespaces:
  run with `--root`, or set `kernel.apparmor_restrict_unprivileged_userns=0`").
- **Failure while applying**: rebuild each affected node from scratch to the *previous* effective
  state; if that also fails, mark the node `degraded` in `show`. The request reports failure with
  the exact command and stderr; recorded state stays at the previous version.
- **Dead supervisor**: rootless leaves nothing behind; a stale lab dir (dead `pid`) is cleaned by
  the next `up`/`down`. Root mode: `up` refuses to start over leftovers and points at
  `tpo down --force`.
- **Node processes** that exit show `exited(code)` and are not restarted.
- **CLI ↔ supervisor**: missing socket = "lab not running"; requests time out after 10s
  (`--timeout`).

## 9. Code layout

| module | responsibility | privileged? |
|---|---|---|
| `model` | TOML parsing, validation, groups, link expansion/merge, `run` resolution | no |
| `plan` | effective edges, diffs, routing, per-node `ip`/`tc` operations | no |
| `backend` | executes operations in a node namespace (trait; impl shells out to `ip`/`tc`/`nft`) | yes |
| `supervisor` | namespaces, mounts, uplink, processes, timers, socket server | yes |
| `cli` | clap commands, socket client, `exec`/`shell` | partly |

`model` and `plan` are pure and hold most of the logic. A netlink backend could later implement
the same trait.

Licence: MIT OR Apache-2.0.

## 10. Testing

1. **Unit (no privileges)**: `model` (parsing, expansion, merge order, `run` specificity and its
   errors, selectors, sides) and `plan` (exact operation lists for state transitions, shortest
   paths, failover).
2. **Golden**: `explain` output for sample labs in `tests/golden/`.
3. **Integration (needs userns; feature/env gated)**: real labs, measured — RTT ≈ delay within
   tolerance, loss roughly matches, direct-mode non-neighbours unreachable, routed failover after
   a cut, `--for` heals on time, `internet` reachable while nodes stay mutually isolated,
   `/etc/hosts` names resolve, killing the supervisor leaves nothing behind. Skips with a stated
   reason when `doctor` fails.
4. **CI**: GitHub Actions `ubuntu-latest`, relaxing the AppArmor userns sysctl via `sudo`, plus a
   job running integration tests under `--root`.
