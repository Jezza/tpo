# tpo — design

Date: 2026-10-02
Status: draft for review (rev 2 — folds in review findings)

## 1. Purpose

`tpo` is a standalone, workload-agnostic CLI (and library) for building virtual network topologies
on a single Linux machine and changing them while they run. You describe nodes and links at a high
level in TOML; `tpo` builds them from namespaces, veths, a bridge and `tc`, runs whatever commands
you want inside each node, and lets you cut, degrade and heal links live.

Motivating use: testing myrmic's behaviour under non-clique topologies, partitions and degraded
links — but nothing in `tpo` knows about myrmic. Any process can run in a node.

Published on crates.io as `tpo` (name verified free 2026-10-02). Linux only.

### Success criteria

- A 5-node chain/star/mesh is a few lines of TOML; `tpo up lab.toml` brings it up rootless.
- Real processes run in nodes; `tpo exec`/`tpo shell` behave like a normal terminal.
- Live changes (`cut`, `set`, `heal`, `apply`, `stop`/`start`) take effect without tearing down
  the lab and without disturbing unrelated links.
- Measured behaviour matches the spec: one-way latency ≈ configured `delay` (so RTT over a link
  with `delay = "20ms"` both ways ≈ 40ms); unlinked pairs exchange no packets at all (IPv4, ARP
  or otherwise).
- Nodes can be given internet access while staying isolated from each other.
- Tearing down, or killing the supervisor, leaves no processes, namespaces or interfaces behind
  (rootless).
- Failures are reported with a cause and a fix, never a bare `EPERM`.
- Test harnesses can drive a lab programmatically (`tpo::Client`, `--json`).

### Non-goals (v1)

IPv6 (disabled inside nodes); restart policies for node processes; topology generators/templates;
netlink backend; pasta uplink; bridging to physical NICs; ICMP to the internet when rootless.

## 2. Privilege model

**Rootless (default).** The supervisor runs `unshare(CLONE_NEWUSER | CLONE_NEWNET)` mapping the
invoking uid to root, which grants `CAP_NET_ADMIN` over namespaces it owns and nothing else. It
does this first thing, while still single-threaded.

Verified on Debian 13 / kernel 7.1.13 (2026-10-02), unprivileged:

- bridge + veth into per-node netns; traffic flows
- `tc` netem, HTB root + per-destination classes, `u32`/`flower` filters, ingress `action drop`,
  `clsact`; qdisc/classifier modules autoload from inside the userns
- per-netns `sysctl net.ipv4.ip_forward`, loopback `/32`s, static routes
- unnumbered veths with `/32`-on-`lo` and `onlink` routes: a→c via b works, source is a's `/32`
- an outside process joining via `setns` on a userns + a netns held only by an fd
- slirp4netns uplink + `nft` masquerade in a gateway netns; node reaches internet over TCP
  (slirp4netns was launched from the host netns — see §6 for how the supervisor does that)
- per-node mount namespace bind-mounting `/etc/resolv.conf` and `/etc/hosts`
- `setns(CLONE_NEWUSER)` / `setns(CLONE_NEWNS)` fail with `EINVAL` from a multithreaded process
  (constraint, see §7.2)

Known environments where rootless fails (detected by `tpo doctor`, see §8):

- Ubuntu ≥ 23.10 with `kernel.apparmor_restrict_unprivileged_userns=1` (userns has no caps)
- `kernel.unprivileged_userns_clone=0` or `user.max_user_namespaces=0`
- inside Docker/CI containers (default seccomp blocks `CLONE_NEWUSER`)
- kernels lacking netem/HTB or with module autoload locked down

**Root (`--root`, opt-in).** Same supervisor and protocol, but nodes are real
`ip netns add tpo-<lab>-<node>` namespaces visible to `ip netns exec` and other tools. Namespaces
outlive a crashed supervisor; `tpo down --force` kills any processes left in them and deletes them
using `state.json`.

`ip`, `tc`, `nft`, `slirp4netns` are invoked by absolute path (resolved once at startup, searching
`/usr/sbin:/sbin:/usr/bin:/bin` and `$PATH`), since `/usr/sbin` is often not on a user's `PATH`.

## 3. Topology file (TOML)

```toml
name   = "chain"          # default: file stem
mode   = "direct"         # "direct" | "routed"
subnet = "10.77.0.0/24"   # default 10.77.0.0/24

[defaults]                # applied to every link before any [[link]]
delay = "1ms"

[nodes]                   # declaration order is preserved
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

Tables are parsed order-preserving (`toml` with `preserve_order` / `IndexMap`).

### 3.1 Names and selectors

- Node and group names share one namespace; a clash is an error. `internet` and `*` are reserved.
  Names must be valid hostnames (`[a-z0-9-]`, ≤ 63 chars).
- A **selector** is `*`, a node name, a group name, or a comma list of those (`a,edge`).
  The same syntax is used in the file and on the CLI.

### 3.2 Links

- Endpoints (`from`, `to`, `path` elements, `mesh`) accept a node, a group, or a list of either;
  groups expand to their members.
  - `path = [x, y, z]`: links x–y and y–z (each step expands bipartitely).
  - `from`/`to`: every member of `from` to every member of `to`.
  - `mesh`: every pair of distinct members.
- Bidirectional unless `oneway = true` (only `from` → `to`; invalid with `path`/`mesh`, and with
  `internet` as an endpoint).
- Self-links are ignored; a link whose expansion is empty is an error.
- Internally the topology is a set of **directed edges** `(from, to)`, each with its own
  properties.

### 3.3 Link properties

All properties apply **per direction**: `delay = "20ms"` on a bidirectional link adds 20ms each
way (RTT +40ms).

| key         | mapping               | example   |
|-------------|-----------------------|-----------|
| `delay`     | netem `delay`         | `"20ms"`  |
| `jitter`    | netem `delay … <jit>` | `"5ms"`   |
| `loss`      | netem `loss`          | `"2%"`    |
| `duplicate` | netem `duplicate`     | `"1%"`    |
| `reorder`   | netem `reorder`       | `"10%"`   |
| `corrupt`   | netem `corrupt`       | `"0.1%"`  |
| `rate`      | netem `rate`          | `"1mbit"` |

Values are parsed and validated at load. `jitter`/`reorder` without `delay` is an error.

**Merge order:** `[defaults]` → each `[[link]]` in file order. Later entries win **per directed
edge, per property**; unspecified properties are inherited. So a later `oneway` entry refines one
direction of an earlier bidirectional link.

### 3.4 Addresses

- Node addresses come from `subnet` in declaration order, skipping explicit `addr`s and the
  addresses reserved for the gateway (`.254`) and bridge. The subnet must fit all nodes. Clashes
  are errors.
- **Allocations are persisted** in `state.json`: on `apply`, existing nodes keep their address;
  new nodes get the lowest free one; removed nodes free theirs. An explicit `addr` that differs
  from the current one changes it.
- A node's **index** (for tc handles/prefs) is the host part of its address, so handles are
  stable across `apply` for free.
- Routed mode has no per-link addressing (unnumbered veths, §4.2). `metric = "hops" | "delay"`
  (default `hops`); with `delay`, `set`/`heal` that change delay also recompute routes.

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
  (uppercased, `-` → `_`). `tpo exec`/`shell` get the same variables.
- stdout/stderr → `<labdir>/logs/<node>.log`.

## 4. Network modes

Common to both modes, in every node: `lo` up; `net.ipv6.conf.all.disable_ipv6=1` (and
`default`); `rp_filter=0` set explicitly.

### 4.1 `direct` — reachability = adjacency

```
        hidden bridge (one L2 segment, `subnet`)
     ┌──────────┼──────────┐
   a .1       b .2       c .3
```

- A hub namespace holds a bridge (`mcast_snooping 0`, so multicast floods without a querier);
  each node has one veth `eth0` onto it.
- **Egress (shaping):** per node, an HTB root used purely as a classifier (every class has a fixed
  very large `rate`/`ceil`; shaping is done by netem `rate`). Default class = unshaped.
  - One class + netem child per directed edge from this node, **always installed** (even if all
    properties are zero), so every runtime property change is a `tc qdisc change`.
  - A `u32` filter on destination address per peer, `pref` = peer index (O(1) delete).
  - Subnet broadcast and `224.0.0.0/4` go to the default class via higher-priority filters, so
    broadcast/multicast are **not** shaped by link properties (documented limitation).
  - Edge to `internet` (§6): its class gets the lowest-priority catch-all filter
    (`dst 0.0.0.0/0`), since internet-bound packets carry real destination addresses.
  - netem `limit` is set large (e.g. 10000 packets, or from rate × delay when both are set) so
    long fat links don't tail-drop at the default 1000.
- **Ingress (reachability) is an allowlist** on `eth0` via `clsact`:
  - accept IPv4 whose source is a node with a live edge *to* this node;
  - accept ARP whose sender IP (`arp_sip`) is such a node;
  - accept anything from the gateway's MAC if this node has a live edge from `internet`;
  - drop everything else (other IPv4, ARP from non-neighbours, IPv6, any other ethertype).

  Filtering at the receiver also confines broadcast/multicast discovery to neighbours, and makes
  non-neighbours invisible even at L2 (`arping` fails).
- No IP forwarding: a node reaches only nodes it has an edge to. Multi-hop delivery is the
  application's job.
- Setup and multi-op changes are applied with `tc -batch` per node (N² filters otherwise means N²
  process spawns).

### 4.2 `routed` — reachability = path

```
   a ──veth── b ──veth── c        unnumbered veths
  10.77.0.1  10.77.0.2  10.77.0.3   (node address /32 on lo)
```

- Each link is a veth pair between the two node namespaces, with **no addresses**; each node's
  address is a `/32` on `lo`; forwarding is enabled in every node.
- Routes: to each neighbour `ip route add <nbr>/32 dev <veth>`; to each further node
  `ip route add <dst>/32 via <next-hop> dev <veth> onlink`, along the shortest path over live
  directed edges (`metric`). The source address is the node's `/32` (verified).
- **`internet` is never a transit hop** in path computation (the gateway drops lab→lab traffic).
- netem on each veth's egress, per direction (same class-per-edge rules as §4.1, one peer per
  veth so no classifier needed).
- **Cut** = per-direction drop filter on the receiving veth's ingress (so `--oneway` works, and
  packets are dropped silently as in direct mode), followed by route recompute.
  - If an alternate path exists, routes move to it.
  - If no path exists, the previous route is **kept**, so packets travel to the cut and are
    dropped there silently (no `ENETUNREACH`, consistent with direct mode).
- Multicast does not cross hops.

## 5. Node environment

Each node has its own **network, mount and UTS** namespace, all inside the lab's user and pid
namespaces (§7.1). In the mount namespace (made `MS_REC|MS_PRIVATE` first):

- `/proc` is remounted for the lab pid namespace (`ps` shows only the lab's processes).
- `/etc/hosts`: every node's name → address, plus `internet` → gateway. Updated **in place**
  (truncate + write, never rename) so the bind mount keeps pointing at it.
- `/etc/resolv.conf`: `nameserver 10.0.2.3` when the lab uses `internet`, otherwise empty.
- `/etc/nsswitch.conf`: `hosts: files dns`, so lookups never reach host daemons (avahi/mDNS,
  systemd-resolved, `myhostname`) over unix sockets. Other databases are copied from the host's.
- hostname (`sethostname`) = node name.

## 6. Internet access

- `internet` is a reserved built-in node, created only if some link references it. It behaves
  like any node for links, faults and properties (`tpo cut a internet` = WAN outage, including
  for already-open connections).
- Implementation: a hidden gateway namespace with a slirp4netns uplink (`tap0`, NAT to the host's
  network), `nft` masquerade on `tap0`, and forwarding enabled.
- **Launching slirp4netns**: the supervisor opens its original `/proc/self/ns/net` *before* its
  own `unshare`. slirp4netns is spawned with a `pre_exec` that `setns`es back to that host netns,
  and run as `slirp4netns --netns-type=path <gateway netns path> -c -r <ready-fd>
  [--disable-host-loopback] tap0`. The supervisor waits on the ready fd before configuring `nft`.
- **Lifecycle**: slirp4netns runs inside the lab pid namespace and dies with it. Its state is
  shown in `tpo show`; if it exits, `internet` is marked `degraded` and its edges stop working.
- **Enforcement** is at the gateway (internet traffic carries non-lab addresses):
  - an nft named set `allowed` holds the addresses of nodes with a live edge to `internet`;
  - the forward chain checks `ip saddr @allowed` (outbound) and `ip daddr @allowed` (inbound)
    **before** any `ct state established` accept, so a cut also kills open connections;
  - lab→lab forwarding is dropped: nodes linked only to `internet` can meet only via something
    outside the lab (e.g. a lighthouse);
  - cut/heal = add/delete a set element, plus adding/removing the node's default route.
- Direct mode: the gateway is on the bridge; linked nodes get `default via <gateway>`. Routed
  mode: `internet` is a router node at the edge of the graph; linked nodes get a default route
  along the shortest path.
- netem for the `internet` edge: on the node's egress toward the gateway (catch-all class, §4.1)
  and on the gateway's egress toward the node.
- `[internet] host_loopback = true` maps the host's `127.0.0.1` to `10.0.2.2` (otherwise
  `--disable-host-loopback`).
- Rootless ICMP to the internet requires the host's `net.ipv4.ping_group_range` to allow it;
  TCP/UDP always work.
- slirp4netns (and `/dev/net/tun`) is a runtime dependency only for labs using `internet`.

## 7. Runtime: supervisor and CLI

One crate (`tpo`, lib + bin). The supervisor is the same binary.

### 7.1 Lifecycle

- `tpo up lab.toml` runs in the foreground (Ctrl-C tears down); `tpo up -d` re-execs as hidden
  `tpo __supervise`, detaches, and returns once the lab is ready.
- Startup, while single-threaded:
  1. open host netns fd (for slirp4netns, §6)
  2. `unshare(CLONE_NEWUSER | CLONE_NEWNET | CLONE_NEWPID)` and write uid/gid maps
  3. fork the **lab init**: `tpo __init`, pid 1 of the lab pid namespace. It reaps zombies and
     has `PR_SET_PDEATHSIG(SIGKILL)` set by the (permanent) main thread of the supervisor. When
     it dies, the kernel SIGKILLs every process in the pid namespace.
- Only then does the supervisor start its async runtime.
- **All forks** (node setup, node processes, `ip`/`tc` invocations, slirp4netns) happen on one
  dedicated, never-exiting spawner thread, so PDEATHSIG and thread-lifetime surprises cannot
  occur. Children `setns` into the target node's namespaces in `pre_exec` (a forked child is
  single-threaded, so this is allowed).
- Node setup: a short-lived child per node unshares net+mnt+uts, sets up mounts/hostname/sysctls,
  and exits after the supervisor has opened fds on its `ns/{net,mnt,uts}`.
- Node processes (from `run`) are started in their own process group; `stop` signals the group.
- Lab directory `$XDG_RUNTIME_DIR/tpo/<lab>/` (root mode: `/run/tpo/<lab>/`): `pid`, `ctl.sock`,
  `state.json` (spec, faults, address allocations), `logs/`.
- `tpo down`: kill the lab init (→ every lab process, including open `tpo exec`/`shell`
  sessions, dies), close fds (→ namespaces and interfaces go away), remove the lab dir.
- Supervisor crash or `kill -9`: the lab init gets PDEATHSIG, so the same cleanup happens via the
  kernel. Nothing is left behind rootless.

### 7.2 Protocol and the CLI

- `ctl.sock` speaks JSON lines. All mutating commands are thin clients: one request, one reply
  with the effective diff or an error. The supervisor serialises requests; only it runs `ip`/`tc`.
- **The CLI is plain blocking Rust (no async runtime)**, so it stays single-threaded, which
  `setns` into user/mount namespaces requires.
- `exec`/`shell`: the CLI asks the supervisor for the node's namespace fds (user, mnt, net, uts,
  pid), received via `SCM_RIGHTS` over `ctl.sock` (works under `hidepid`), then:
  1. `setns` user → mnt → net → uts → pid, before anything could spawn a thread
  2. `chdir` back to the original cwd (`setns(mnt)` resets it to `/`; falls back to `/`)
  3. fork (the pid namespace applies to children) and exec the command in the child with the
     `TPO_*` environment; the parent waits and exits with the child's status.

  tty, stdio and signals are inherited normally.

### 7.3 State: spec + faults

- **Spec**: the topology from the file. Changed only by `up`/`apply`.
- **Faults**: runtime changes, stored per expanded directed edge (cut flag, property overrides,
  optional expiry). Faults can only cut or modify edges the spec has; they never add edges.
- Faults survive `apply` (and are dropped for edges that no longer exist).
  `tpo heal --all` returns exactly to the spec.

### 7.4 Commands

Sides/selectors use the §3.1 syntax. Every command accepts `--json` for machine-readable output.

| command | effect |
|---|---|
| `tpo up <file> [-d] [--root]` | start a lab |
| `tpo down [--force]` | stop a lab (`--force`: clean up after a dead root-mode supervisor) |
| `tpo apply <file>` | replace the spec: add/remove nodes and links, change properties; a node's process restarts only if its resolved `run` command changed |
| `tpo cut <side> [<side>…] [--oneway] [--for <dur>]` | cut every edge between different sides |
| `tpo set <side> [<side>…] k=v… [--oneway] [--for <dur>]` | override properties on existing edges between the sides; `k=` drops the override for `k`, `k=0` forces zero |
| `tpo heal <side> [<side>…]` / `tpo heal --all` | remove faults on edges between the sides |
| `tpo stop <sel> [--signal SIG] [--grace 5s] [--for <dur>]` | stop node processes (SIGTERM → SIGKILL after grace); network untouched |
| `tpo start <sel>` / `tpo restart <sel>` | start/restart node processes |
| `tpo exec <node> -- <cmd…>` / `tpo shell <node>` | run a command / interactive shell in a node |
| `tpo show` | nodes, process states, spec, faults, effective edges, `degraded` markers |
| `tpo explain <node>` | the `ip`/`tc`/`nft` state the plan expects for a node |
| `tpo resync [<sel>]` | rebuild nodes from scratch to the current effective state |
| `tpo logs <node> [-f]` | node process logs |
| `tpo doctor` | preflight checks (§8) |

Lab selection: `--lab <name>`; defaults to the only running lab, error if several.

Side rules (`cut`/`set`/`heal`):

- Each argument is a side; the command applies to every edge between two *different* sides.
  Edges within a side are untouched.
- One side means `<side> *` (e.g. `tpo cut a` isolates `a`; `tpo cut edge` cuts the group off but
  keeps its internal links; `tpo cut d e *` isolates each).
- Sides must be disjoint; `*` means every node not named in another side (including `internet`
  if present).
- `--oneway` requires exactly two sides (first → second).
- No edges between the sides is an error.

Process commands (`stop`/`start`/`restart`) take one selector, report per node
(`already running`, `no command`, `exited(1)` …) and act in parallel; only unknown names are
errors.

### 7.5 Reconciliation

Every request:

1. validate against the model (no side effects on failure)
2. compute the effective directed-edge set (spec ⊕ faults)
3. diff against the previous effective set
4. emit minimal per-edge operations
   - direct: `tc qdisc change` (netem); add/remove one ingress allow filter (cut/heal);
     add/remove a peer's class + netem + filters (apply)
   - routed: `tc qdisc change` on the veth; add/remove the ingress drop on the receiving veth;
     recompute shortest paths; `ip route replace`/`del` only for changed routes
   - internet: nft set element add/delete; add/remove the node's default route
5. apply (batched per node), and return the diff (`a→b delay 20ms→200ms`, `cut a⇄b`,
   `route a→c via b → via d`)

Per-node operations are idempotent: any node can be rebuilt from scratch to a given effective
state (`resync`).

## 8. Errors

- **Validation first**: unknown names, `run` ambiguities, overlapping sides, empty cuts,
  malformed values, `oneway` to `internet`, address clashes, undersized subnets → precise error,
  nothing changed.
- **`tpo doctor`** (also run by `up`) probes userns creation, caps inside it, veth/bridge,
  netem/HTB/u32/flower, mount and pid namespaces, tool paths, and slirp4netns + `/dev/net/tun` +
  nft when `internet` is used. Each failure maps to a cause and fix (e.g. "AppArmor restricts
  unprivileged user namespaces: run with `--root`, or set
  `kernel.apparmor_restrict_unprivileged_userns=0`").
- **Failure while applying**: resync each affected node to the *previous* effective state. If that
  also fails, mark the node `degraded` in `show` (fixable with `tpo resync`). The request reports
  failure with the exact command and stderr; recorded state stays at the previous version.
- **Dead supervisor**: rootless leaves nothing behind (§7.1); a stale lab dir (dead `pid`) is
  cleaned by the next `up`/`down`. Root mode: `up` refuses to start over leftovers and points at
  `tpo down --force`.
- **Node processes** that exit show `exited(code)` and are not restarted.
- **CLI ↔ supervisor**: missing socket = "lab not running"; requests time out after 10s
  (`--timeout`).

## 9. Code layout and library API

| module | responsibility | privileged? |
|---|---|---|
| `model` | TOML parsing, validation, groups, link expansion/merge, `run` resolution, selectors/sides | no |
| `plan` | effective edges, diffs, routing, per-node `ip`/`tc`/`nft` operations | no |
| `backend` | executes operations in a node namespace (trait; impl shells out, batched) | yes |
| `supervisor` | namespaces, lab init, spawner thread, mounts, uplink, processes, timers, socket server | yes |
| `client` | blocking `ctl.sock` client | no |
| `cli` | clap commands over `client`, `exec`/`shell` | partly |

**Public library API** (for test harnesses such as myrmic's):

- `tpo::model::Topology` — parse/validate a file or build one in code.
- `tpo::Client` — connect to a running lab by name; typed methods mirroring the CLI (`up`,
  `apply`, `cut`, `set`, `heal`, `stop`/`start`/`restart`, `show`, `down`) returning the same
  structures as `--json`.
- The `ctl.sock` JSON protocol is versioned (`"v": 1` in every message).

`model` and `plan` are pure and hold most of the logic. A netlink backend could later implement
the `backend` trait.

Licence: MIT OR Apache-2.0.

## 10. Testing

1. **Unit (no privileges)**: `model` (parsing, expansion, merge order, `run` specificity and its
   errors, selectors, sides, address allocation across `apply`) and `plan` (exact operation lists
   for state transitions, shortest paths with `internet` excluded as transit, failover, kept
   routes when no path exists).
2. **Golden**: `explain` output for sample labs in `tests/golden/`.
3. **Integration (needs userns; feature/env gated)**: real labs, measured:
   - one-way delay ≈ configured (RTT ≈ sum of both directions) within tolerance; loss roughly
     matches;
   - direct-mode non-neighbours exchange nothing: IPv4, ARP (`arping`), and raw frames;
   - routed multi-hop works, failover after a cut, `--oneway` cut;
   - `--for` heals on time;
   - `internet`: reachable over TCP while nodes stay mutually isolated; `cut a internet` kills an
     open connection;
   - names resolve from `/etc/hosts`, and lookups never leave the node (no host avahi/resolved);
   - `kill -9` of the supervisor and `tpo down` leave no processes, namespaces or interfaces,
     including with an open `tpo shell`.

   Skips with a stated reason when `doctor` fails.
4. **CI**: GitHub Actions `ubuntu-latest`, relaxing the AppArmor userns sysctl via `sudo`, plus a
   job running integration tests under `--root`.
