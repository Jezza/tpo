# tpo — design

Date: 2026-10-02
Status: implemented (rev 7 — §11 records where the implementation departs from rev 6)

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
  with `delay = "20ms"` both ways ≈ 40ms); unlinked pairs exchange no frames at all (IPv4, IPv6,
  ARP/NDP or otherwise).
- Nodes can be given internet access while staying isolated from each other.
- Tearing down, or killing the supervisor, leaves no processes, namespaces or interfaces behind
  (rootless).
- Failures are reported with a cause and a fix, never a bare `EPERM`.
- Test harnesses can drive a lab programmatically (`tpo::Client`, `--json`).

### Non-goals (v1)

Restart policies for node processes; topology generators/templates; netlink backend; slirp4netns
uplink; bridging to physical NICs; ICMP to the internet when rootless.

## 2. Privilege model

**Rootless (default).** The supervisor itself **stays in the host's namespaces**. It creates the
lab by `clone`-ing a lab init process into new user, network and pid namespaces, mapping the
invoking uid to root inside. That grants `CAP_NET_ADMIN` over namespaces the lab owns and nothing
else. The supervisor then works inside the lab only through forked, single-threaded children
that `setns` into the lab's namespaces (§7.1). It must stay outside because a process inside a
user namespace cannot `setns` back into the host netns (`EPERM`, verified), and the pasta uplink
(§6) has to run in the host netns.

Verified on Debian 13 / kernel 7.1.13 (2026-10-02), unprivileged:

- bridge + veth into per-node netns; traffic flows
- `tc` netem, HTB root + per-destination classes, `u32`/`flower` filters, ingress `action drop`,
  `clsact`; qdisc/classifier modules autoload from inside the userns
- per-netns `sysctl net.ipv4.ip_forward`, loopback `/32`s, static routes
- unnumbered veths with `/32`-on-`lo` and `onlink` routes: a→c via b works, source is a's `/32`
- an outside process joining via `setns` on a userns + a netns held only by an fd
- pasta uplink (launched from the host netns) + `nft` `inet` masquerade in a gateway netns:
  a node on the lab's private v4/ULA v6 addresses reaches the internet over **IPv4 and IPv6**, by
  IP and by name, HTTP and HTTPS
- slirp4netns 1.3.1 IPv6 is unusable (HTTPS over v6 timed out; RA races static config) —
  rejected
- `setns` from inside the user namespace back to the host netns fails with `EPERM` (why the
  supervisor stays outside)
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

`ip`, `tc`, `nft`, `pasta` are invoked by absolute path (resolved once at startup, searching
`/usr/sbin:/sbin:/usr/bin:/bin` and `$PATH`), since `/usr/sbin` is often not on a user's `PATH`.

## 3. Topology file (TOML)

```toml
name   = "chain"          # default: file stem
mode   = "direct"         # "direct" | "routed"
subnet = "10.77.0.0/24"   # default 10.77.0.0/24
subnet6 = "fd77::/64"     # default fd77::/64 (ULA)

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
host_loopback = false     # true: host's loopback reachable as `host` (192.0.2.2 / 2001:db8::2)

[run]
"*"    = "myrmic runtimes start --tmp --name $TPO_NODE"
"edge" = "iperf3 -s"
"c"    = ""               # run nothing on c
```

Tables are parsed order-preserving (`toml` with `preserve_order` / `IndexMap`).

### 3.1 Names and selectors

- Node and group names share one namespace; a clash is an error. `internet`, `host` and `*` are
  reserved.
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

- Nodes are **dual-stack**. IPv4 addresses come from `subnet` in declaration order, skipping
  explicit `addr`s and the address reserved for the gateway (`.254`, or the last usable host
  address of a larger subnet). The subnet must fit all nodes and must be `/17` or longer (indices
  must fit tc's 16-bit minor/pref space). Clashes are errors.
- Each node's IPv6 address is `subnet6` + the same host index (`fd77::1` for `10.77.0.1`); an
  explicit `addr6` overrides it.
- Every node interface tpo creates has a deterministic, locally administered MAC
  `02:74:<lab hash, 16 bits>:<own index, 16 bits>`. Routed-mode veths use
  `02:74:<(lab hash + peer index) mod 2^16>:<own index>`, so the two ends of a pair (and a node's
  veths to different peers) are distinct. Filters key on these MACs (§4.1) and neighbour entries are
  static (§4).
- **Allocations are persisted** in `state.json`: on `apply`, existing nodes keep their addresses;
  new nodes get the lowest free one; removed nodes free theirs. An explicit `addr` that differs
  from the current one changes it.
- A node's **index** is the host part of its IPv4 address (1 … 65533), so handles are stable
  across `apply` for free. tc layout per node: HTB root `1:`, default class `1:fffe`, per-peer
  class `1:<idx>`, its netem addressed as `parent 1:<idx>` (kernel-assigned handle), peer filters
  `pref <idx>`, the final drop rule `pref 65535`. HTB `quantum` is set explicitly so the large
  classifier rates don't trigger kernel warnings.
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
- Environment: `TPO_LAB`, `TPO_NODE`, `TPO_ADDR`, `TPO_ADDR6`, and `TPO_ADDR_<NODE>` /
  `TPO_ADDR6_<NODE>` for every node
  (uppercased, `-` → `_`). `tpo exec`/`shell` get the same variables.
- stdout/stderr → `<labdir>/logs/<node>.log`.

## 4. Network modes

Common to both modes, in every node: `lo` up; IPv6 enabled; `rp_filter=0` set explicitly;
IPv6 DAD disabled (`accept_dad=0`, addresses are unique by construction) so addresses are usable
immediately. Every interface tpo creates gets an explicit link-local `fe80::<own index>/64`
(`addr_gen_mode none`), so link-local addresses never depend on EUI-64.

**Static neighbours.** Because every MAC is known, tpo installs **permanent neighbour entries**
(`ip neigh replace <addr> lladdr <mac> nud permanent`, for IPv4, IPv6 global and IPv6 link-local)
instead of relying on ARP/NDP: in direct mode for every lab node and the gateway on every node;
in routed mode for each veth's peer. This is required for one-way edges: with dynamic resolution
an a→b-only edge delivers nothing (b's ARP/NDP replies are dropped at a), and a `--oneway` cut
decays into a bidirectional one once neighbour entries go stale (verified). It also means no
ARP/NDP traffic, so `explain`/golden output is deterministic.

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
  - Classification is by **destination MAC** (`flower dst_mac`, `protocol all`), one filter per
    peer, `pref` = peer index (O(1) delete). This is protocol-agnostic: IPv4, IPv6, ARP and NDP
    to a peer all take that peer's class.
  - Broadcast and multicast MACs match no peer filter and fall to the default class, so they are
    **not** shaped by link properties (documented limitation).
  - Edge to `internet` (§6): its class matches the gateway's MAC, which every internet-bound
    frame carries regardless of its destination IP.
  - netem `limit` is set large (e.g. 10000 packets, or from rate × delay when both are set) so
    long fat links don't tail-drop at the default 1000.
- **Ingress (reachability) is an allowlist by source MAC** on `eth0` via `clsact`
  (`flower src_mac`, `protocol all`):
  - accept frames from the MAC of each node with a live edge *to* this node;
  - accept frames from the gateway's MAC if this node has a live edge from `internet`;
  - drop everything else.

  One rule per neighbour covers IPv4, IPv6, ARP, NDP and anything else. Filtering at the
  receiver also confines broadcast/multicast discovery (including IPv6 link-local and mDNS) to
  neighbours, and makes non-neighbours invisible at L2 (`arping`/`ndisc6` fail). Verified with
  IPv4, IPv6 global and link-local: neighbours get the configured delay, non-neighbours get
  nothing, `ping ff02::1` answers only from neighbours.
- Because ingress is default-drop, adding a node with `apply` is order-safe: its frames are dropped
  everywhere until its allow rules are installed.
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
- IPv6 likewise: each node's `/128` on `lo`; veths carry only their `fe80::<own index>/64` (§4);
  routes are `ip -6 route add <dst>/128 via fe80::<next-hop index> dev
  <veth>`.
- **`internet` is never a transit hop** in path computation (the gateway drops lab→lab traffic).
- netem on each veth's egress, per direction (same class-per-edge rules as §4.1, one peer per
  veth so no classifier needed).
- **Cut** = per-direction drop-all filter (`protocol all`) on the receiving veth's ingress (so `--oneway` works, and
  packets are dropped silently as in direct mode), followed by route recompute.
  - If an alternate path exists, routes move to it.
  - If no path exists, the previous route is **kept**, so packets travel to the cut and are
    dropped there silently (no `ENETUNREACH`, consistent with direct mode).
- Multicast does not cross hops.

## 5. Node environment

Each node has its own **network, mount and UTS** namespace, all inside the lab's user and pid
namespaces (§7.1). In the mount namespace (made `MS_REC|MS_PRIVATE` first):

- `/proc` is remounted for the lab pid namespace (`ps` shows only the lab's processes).
- `/etc/hosts`: every node's name → IPv4 and IPv6 address, plus `internet` → gateway, plus
  `host` → the host-loopback addresses when `host_loopback = true`. Updated **in place**
  (truncate + write, never rename) so the bind mount keeps pointing at it.
- `/etc/resolv.conf`: the DNS-forward addresses for the address families the host actually has
  (as found by `doctor`; IPv4 `192.0.2.53` first, then IPv6 `2001:db8::53`) when the lab uses
  `internet`, otherwise empty. Listing a family the host lacks would make every lookup wait out
  glibc's timeout.
- `/etc/nsswitch.conf`: `hosts: files dns`, so lookups never reach host daemons (avahi/mDNS,
  systemd-resolved, `myhostname`) over unix sockets. Other databases are copied from the host's.
- hostname (`sethostname`) = node name.

## 6. Internet access

- `internet` is a reserved built-in node, created only if some link references it. It behaves
  like any node for links, faults and properties (`tpo cut a internet` = WAN outage, including
  for already-open connections). It is **dual-stack**: IPv4 and IPv6 work equally.
- Implementation: a **separate gateway namespace** (not the hub), created like a node, with a
  **pasta** uplink, `nft` `inet` masquerade (IPv4 NAT and NAT66) on the uplink interface, and
  IPv4 + IPv6 forwarding enabled.
  - In direct mode its veth `gw0` sits on the bridge and is treated **exactly like a node**:
    index/MAC of the gateway address, per-peer egress classes (the "gateway → node" direction of
    each `internet` edge), and a src-MAC ingress allowlist of nodes with a live edge to
    `internet`. The nft sets below are then defence in depth.
  - In routed mode it is the `internet` router node, with veths to linked nodes.
  - Its nft `input` chain accepts lab traffic only from `allowed4`/`allowed6` and drops the
    rest, so a node without an `internet` edge can't even ping the gateway.
  - Like nodes, its netns is held by a long-lived holder process inside the lab pid namespace
    (§7.1), so the netns has a `/proc` path that disappears with the lab.
- **pasta** (from `passt`, rootless Podman's default) copies the host's real addresses and routes
  into the gateway's `tap0` and translates its traffic to host sockets. Lab traffic is
  masqueraded to those addresses, so it leaves the host as ordinary host traffic over whatever
  IPv4/IPv6 connectivity the host has.
- **Launching pasta**: the supervisor (in the host netns, §2) spawns
  `pasta --config-net -I tap0 --userns /proc/<init pid>/ns/user
  --netns /proc/<gateway holder pid>/ns/net
  --dns-forward 192.0.2.53 --dns-forward 2001:db8::53 --map-host-loopback <none|192.0.2.2>
  -P <pidfile>` and waits for it to exit 0 (it daemonises once `tap0` is configured) before
  configuring `nft`. Exact flags (e.g. the IPv6 host-loopback mapping) are pinned during
  implementation against the pasta version `doctor` finds.
- DNS: nodes' `resolv.conf` points at the DNS-forward addresses, which pasta maps to the host's
  resolver (works even when the host resolver is on `127.0.0.1`). These addresses are from the
  documentation ranges and route via the gateway's default route, so they never collide with lab
  addresses.
- **Lifecycle**: the supervisor tracks pasta by pidfile/pidfd and kills it on `down`. If the
  supervisor is `kill -9`ed that kill never runs, so pasta is given `/proc/<pid>/ns/...` paths of
  lab processes (not supervisor fd paths): when the lab pid namespace dies, those paths vanish and
  pasta, which watches its netns path, exits. If pasta dies, `internet` is marked `degraded` in
  `tpo show` and its edges stop working.
- **Enforcement** is at the gateway (internet traffic carries non-lab addresses):
  - nft named sets `allowed4`/`allowed6` hold the addresses of nodes with a live edge to
    `internet`;
  - the forward chain checks `saddr` (outbound) and `daddr` (inbound) against them **before** any
    `ct state established` accept, so a cut also kills open connections;
  - lab→lab forwarding is dropped: nodes linked only to `internet` can meet only via something
    outside the lab (e.g. a lighthouse);
  - cut/heal = add/delete set elements, plus adding/removing the node's IPv4 and IPv6 default
    routes.
- Direct mode: the gateway is on the bridge; linked nodes get `default via` the gateway's v4 and
  v6 addresses. Routed mode: `internet` is a router node at the edge of the graph; linked nodes
  get default routes along the shortest path.
- netem for the `internet` edge: on the node's egress toward the gateway (gateway-MAC class, §4.1)
  and on the gateway's egress toward the node.
- `[internet] host_loopback = true` maps `192.0.2.2` / `2001:db8::2` (resolvable as `host`) to the
  host's loopback, so a service on the laptop (e.g. a local lighthouse) is reachable.
- If the host has no IPv6 (or no IPv4) connectivity, that family is unavailable in the lab too;
  `doctor` reports which families the host has, and warns when a host route that pasta copies
  into the gateway overlaps `subnet`/`subnet6`.
- Rootless ICMP to the internet requires the host's `net.ipv4.ping_group_range` to allow it;
  TCP/UDP always work.
- pasta and `/dev/net/tun` are runtime dependencies only for rootless labs using `internet`.
- **Root mode uses pasta too, by default** (decided 2026-10-05). pasta runs fine as root and
  needs no host-side changes, so a crashed root lab leaves no sysctls, routes or firewall rules
  behind. The differences from rootless: no `--userns`; the lab dir is `/run/tpo/<lab>/`; pasta
  gets the gateway's `/run/netns/tpo-<lab>-gateway` path (as root it drops privileges before
  opening the netns and can't open another process's `/proc/<pid>/ns/net`); and pasta runs with
  `--runas 0` (switching to `nobody` loses the capability to enter a netns owned by the host's
  user namespace; verified on pasta from Debian 13, 2026-10-05).
- **`--kernel-nat` (root only, opt-in)** uses the kernel instead of pasta, for throughput and
  ICMP:
  - The gateway gets a veth pair into the host netns (`tpo<hash>up` on the host, `up0` in the
    gateway), unnumbered on both sides with static neighbours: the gateway's default routes are
    `via 169.254.0.1 dev up0 onlink` and `via fe80::1 dev up0`; the host routes
    `<subnet>`/`<subnet6>` back via the gateway (`onlink`, static neighbour).
  - The host does NAT in an `nft` table `tpo_<hash>`: `ip saddr <subnet> oifname != "tpo<hash>up"
    masquerade` (and `ip6 saddr <subnet6>`), so it keeps working when the host's uplink changes.
    The gateway's own ruleset keeps the `allowed` sets on `up0` but does no NAT.
  - DNS: `192.0.2.53` is DNATed to the host's first IPv4 resolver; a loopback resolver
    (`127.0.0.1`, `127.0.0.53`) needs `route_localnet=1` on `tpo<hash>up`. IPv6 has no
    `route_localnet`, so the IPv6 DNS address is DNATed only to a non-loopback IPv6 resolver and
    omitted from the lab's `resolv.conf` otherwise. `host_loopback`: DNAT `192.0.2.2 → 127.0.0.1`
    (same `route_localnet`).
  - Forwarding: `net.ipv4.ip_forward=1`; for IPv6, `accept_ra=2` is set on the host's interfaces
    that accept RAs *before* `net.ipv6.conf.all.forwarding=1`, so the host keeps its SLAAC
    default route. Previous values are recorded in `state.json` and restored on `down` /
    `down --force`, unless another `--kernel-nat` lab is still running.
  - Host firewalls: a `drop` in any forward-hook chain (Docker's `FORWARD DROP`, ufw, firewalld)
    overrides tpo's accept; `doctor --kernel-nat` detects this and prints the fix.
  - `down` (and `down --force`) remove the veth, routes and nft table and restore the sysctls.
- **Decision — run pasta as a binary, don't link a library.** This is how rootless Podman
  (`containers/common`) and rootlesskit use pasta/slirp4netns. passt has no library API and is
  GPL-2.0-or-later. The linkable alternatives were rejected: libslirp (slower, weak IPv6),
  smoltcp (a stack, not a translator — we'd own the TCP translation edge cases),
  ipstack/netstack-smoltcp (immature), gVisor netstack (Go). Revisit a smoltcp-based built-in
  uplink behind the same interface only if the pasta dependency becomes a real problem.

## 7. Runtime: supervisor and CLI

One crate (`tpo`, lib + bin). The supervisor is the same binary.

### 7.1 Lifecycle

- `tpo up lab.toml` runs in the foreground (Ctrl-C tears down); `tpo up -d` re-execs as hidden
  `tpo __supervise`, detaches, and returns once the lab is ready.
- The supervisor process **stays in the host namespaces** for its whole life (§2).
- Startup, from the supervisor's main thread before its async runtime starts:
  1. `clone(CLONE_NEWUSER | CLONE_NEWNET | CLONE_NEWPID)` the **lab init** (`tpo __init`), which
     is pid 1 of the lab pid namespace; write its uid/gid maps.
  2. The init sets `PR_SET_PDEATHSIG(SIGKILL)` (its parent thread is the supervisor's permanent
     main thread). To close the clone→prctl race it then `poll()`s a pipe whose write end the
     supervisor holds: `POLLHUP` means the supervisor is gone, so it exits. (`getppid()` can't be
     used: pid 1 of a new pid namespace always sees parent pid 0 — verified.) The same pipe
     carries the "uid/gid maps written" go-ahead. After that it just reaps zombies. When it dies, the kernel SIGKILLs every process in
     the lab pid namespace.
  3. The supervisor opens fds on the init's `ns/{user,net,pid}`; the init's netns is the **hub**
     (the bridge only). The gateway has its own netns (§6).
- **All forks** (node setup, node processes, `ip`/`tc`/`nft` invocations, pasta) happen on one
  dedicated, never-exiting spawner thread, so PDEATHSIG and thread-lifetime surprises cannot
  occur.
  - `ip`/`tc`/`nft`: the forked child `setns`es user → net (target node or hub) in `pre_exec` (a
    forked child is single-threaded, so joining the user namespace is allowed) and execs.
  - pasta: spawned without any `setns`; it joins the gateway netns itself (§6).
  - Node setup: a child joins the lab user + pid namespaces via `tpo __enter`, unshares
    net + mnt + uts, sets up mounts/hostname/sysctls, and stays as a tiny holder until the
    supervisor has opened fds on its `ns/{net,mnt,uts}`, then exits. The process that unshares is
    the `__enter` grandchild (after `setns(pid)` + fork), so the wrapper reports that child's
    host-visible pid to the supervisor over a pipe. The gateway's holder is the
    exception: it stays alive for the lab's lifetime (pasta needs a `/proc` path, §6). Verified:
    a holder joining user+pid from the host, then unsharing net+mnt+uts, remounting `/proc` and
    setting the hostname works, and `kill -9` of init kills it.
  - Node processes (from `run`): `tpo __enter <node> -- sh -c <cmd>` (§7.2), in their own
    process group; `stop` signals the group.
- Lab directory `$XDG_RUNTIME_DIR/tpo/<lab>/` (root mode: `/run/tpo/<lab>/`): `pid`, `ctl.sock`,
  `state.json` (spec, faults, address allocations), `logs/`.
- `tpo down`: kill pasta and the lab init (→ every lab process, including open `tpo exec`/`shell`
  sessions, dies), close fds (→ namespaces and interfaces go away), remove the lab dir.
- Supervisor crash or `kill -9`: the lab init gets PDEATHSIG, so the same cleanup happens via the
  kernel; pasta exits when its netns disappears. Nothing is left behind rootless.
- Root mode: same structure without the user namespace; nodes' netns are additionally bind-mounted
  under `/run/netns/tpo-<lab>-<node>` by the supervisor itself (it is still in the host mount
  namespace), via `ip netns attach <name> <holder pid>` while the holder is alive, so iproute2's
  shared `/run/netns` mount conventions are followed.

### 7.2 Protocol and the CLI

- `ctl.sock` speaks JSON lines. All mutating commands are thin clients: one request, one reply
  with the effective diff or an error. The supervisor serialises requests; only it runs `ip`/`tc`.
- **The CLI is plain blocking Rust (no async runtime)**, so it stays single-threaded, which
  `setns` into user/mount namespaces requires.
- `exec`/`shell` and the supervisor's node processes share one routine, `tpo __enter`: get the
  node's namespace fds (user, mnt, net, uts, pid) — the CLI receives them via `SCM_RIGHTS` over
  `ctl.sock` (works under `hidepid`), the supervisor passes them as inherited fds — then:
  1. `setns` user → mnt → net → uts → pid, before anything could spawn a thread
  2. `chdir` back to the original cwd (`setns(mnt)` resets it to `/`; falls back to `/`)
  3. fork (the pid namespace applies to children) and exec the command in the child with the
     `TPO_*` environment; the parent forwards SIGTERM/SIGHUP to the child (and ignores SIGINT/SIGQUIT,
     which the tty already delivers to the child's foreground process group), waits,
     and exits with the child's status (128 + n if it died from signal n).

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
  netem/HTB/flower, mount and pid namespaces, tool paths, and pasta + `/dev/net/tun` + nft +
  host IPv4/IPv6 connectivity when `internet` is used. Each failure maps to a cause and fix (e.g. "AppArmor restricts
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
   - direct-mode non-neighbours exchange nothing: IPv4, IPv6 (incl. link-local multicast),
     ARP (`arping`), NDP, and raw frames; IPv6 between neighbours gets the link's delay;
   - routed multi-hop works over IPv4 and IPv6, failover after a cut, `--oneway` cut;
   - one-way edges carry unicast (UDP over IPv4 and IPv6) in both modes, and a `--oneway` cut
     stays one-way after neighbour timeouts would have expired;
   - `--for` heals on time;
   - `internet`: reachable over IPv4 and IPv6 (TCP/UDP, by name and by IP) while nodes stay
     mutually isolated; `cut a internet` kills an open connection on both families;
     `host_loopback` reaches a listener on the host;
   - names resolve from `/etc/hosts`, and lookups never leave the node (no host avahi/resolved);
   - `kill -9` of the supervisor and `tpo down` leave no processes (including pasta), namespaces
     or interfaces,
     including with an open `tpo shell`.

   Skips with a stated reason when `doctor` fails.
4. **CI**: GitHub Actions `ubuntu-latest`, relaxing the AppArmor userns sysctl via `sudo`, plus a
   job running integration tests under `--root`.

## 11. Implementation notes (2026-10-02)

Where the v1 implementation departs from or pins down the design above:

- **No spawner thread, no async runtime.** Only the lab init carries `PR_SET_PDEATHSIG`, and
  it is cloned from the supervisor's main thread, which lives as long as the lab; every other
  lab process dies with the pid namespace. So the supervisor uses plain threads (one per
  connection, a signal thread, a fault-timer thread) and forks from any of them.
- **Holders.** Node holders exit once the supervisor holds their namespace fds. Only the
  gateway's holder stays alive (pasta needs `/proc/<pid>/ns/net`).
- **Peer namespaces for `ip`.** Routed-mode veth pairs name the peer's netns with an inherited
  fd: planned operations say `netns @netns:<node>`, and the backend replaces that with
  `/proc/self/fd/<n>` for an fd it inherits into `ip` (verified: iproute2 accepts paths).
- **`ip -batch`** does not take per-line options, so planned operations never use `-6`: the
  address family comes from the addresses (`neigh replace <v6>`, `route replace ::/0`).
- **Removed endpoints** get an explicit `ip link del` of their bridge port (direct) or veth
  (routed) before anything else in the same diff: namespace teardown is asynchronous, and a
  new node may reuse the index, and so the interface name, in the same `apply`.
- **`apply`** can't change `mode` or an existing node's address; both need a restart.
- **`host_loopback`** maps IPv4 only (`192.0.2.2`, resolvable as `host`).
- **Supervisor log** is `$XDG_RUNTIME_DIR/tpo/<lab>.log`, outside the lab dir, so it survives
  teardown. `up` refuses runtime dirs whose control-socket path exceeds `sun_path`.

