# Stage 7 notes: networking

The engineering record of stage 7: what was built, why it's built that way, what the kernel
was checked to do, what can go wrong and how that's handled, and what was left for later
stages. For how to *use* it, see [Networking](../user/networking.md) and [Running
commands](../user/run.md).

## Outcome

Stage 7 asked for networking: network namespaces, bridges and veth pairs over netlink, IP
address management, NAT, published ports, rootless networking and an embedded DNS
([ROADMAP](../ROADMAP.md), [FEATURES §7](../FEATURES.md#7-networking--stage-7)). A container
now has a network of its own by default:

- **as root**, on the `bridge` network (`shipyard0`), with an address, a way out through
  NAT, and ports published with `-p`; or on networks made with `shipyard network create`,
  where containers find each other by name, can be on several at once, and can have IPv6;
- **rootless**, through pasta, one per container: a way out, published ports and the
  host's DNS, and never the host's loopback;
- `--network none` (only `lo`) and `--network host` (the host's), as Docker's.

It completes milestone **v0.1**: a normal user's `shipyard run --image alpine sh` is pulled
from Docker Hub, isolated, resource-limited and networked, on the caller's own terminal.
Docker's spelling, `run -it alpine sh`, needs a pseudo-terminal of the container's own
(stage 8) and the image as the first argument (stage 9); that was decided when the
milestone test was written (2026-10-08). The milestone is proved by
`the_v0_1_milestone_a_rootless_interactive_shell_in_a_pulled_image_is_networked`, and was
run by hand with real Docker Hub (see [Tests](#tests)).

Everything talks to the kernel through Shipyard's own netlink code: links, addresses and
routes (rtnetlink), the firewall (nf_tables) and connection tracking (ctnetlink). There's
no `ip` or `nft` binary and no netlink crate.

The stage was built in eight steps, each planned, built, reviewed twice (clean code, then
what could go wrong), mutation-checked and run by hand on Linux, then reviewed as a whole
(see [Review history](#review-history)).

| Requirement | Where | Tested by |
|---|---|---|
| Netlink: a message builder and a bounded parser, requests, dumps (redone when interrupted), extended ACKs, batches | `shipyard-network/netlink/` (`message.rs`, `conversation.rs`, `reply.rs`, `socket.rs`) | `conversation.rs`'s 27 tests against a scripted kernel (`an_interrupted_dump_is_redone_from_scratch`, `a_datagram_from_another_process_is_dropped_unread`, `a_refused_item_names_itself_and_the_rest_is_read_without_waiting`, …); `reading_any_bytes_is_safe`; the `netlink` fuzz target |
| rtnetlink: links, bridges, veth peers born in another namespace, addresses, routes; a socket made inside another namespace | `shipyard-network/route/` | `route/linux_tests.rs` (`a_veth_peer_is_born_in_another_namespace`, `a_dump_longer_than_a_datagram_is_read_whole`, …), `interface_names_follow_the_kernels_rules` |
| Network namespaces in the runtime: `lo` up, joined by path, `netDevices`, a sysfs of its own, `net.*` sysctls | `shipyard-runtime/process/network.rs`, `isolation.rs`, `rootfs.rs` | `tests/network.rs` (`a_network_of_its_own_has_only_loopback`, `its_sysfs_lists_its_own_loopback_and_it_is_up`, `a_bundle_joins_a_network_namespace_by_its_path`, `the_hosts_devices_move_in_renamed`, `network_settings_need_a_network_of_its_own`) |
| Networks and leases: subnets that avoid the host's routes, `flock`-held leases, sweeps after a killed run | `shipyard-network/store.rs` | `store.rs`'s 24 tests (`leases_are_held_released_and_swept`, `creates_at_once_choose_different_subnets`, `a_lock_file_made_again_is_the_one_locked`, …), `a_killed_runs_address_is_swept_by_the_next` |
| Bridge and veth: the container's interface born in its namespace with its final name and MAC; hairpin; a fixed bridge MAC | `shipyard-network/attach.rs`, the CLI's `network.rs` (`Attachment`) | `a_container_is_connected_and_a_failed_connect_leaves_nothing`, `the_host_reaches_containers_on_a_network`, `a_container_on_the_default_network_has_its_address_and_default_route` |
| `/etc/hosts`, `/etc/hostname`, `/etc/resolv.conf`; the run ID as hostname | `shipyard-network/files.rs`, the CLI's `network.rs` (`RunDir`) | `files.rs`'s tests, `the_etc_files_follow_dns_and_reach_a_mapped_root`, `a_root_filesystem_gets_the_etc_files_it_has`, `the_hostname_is_the_runs_id_with_a_network_of_its_own` |
| The firewall: one `inet shipyard` table, masquerade, published ports through DNAT maps, hairpin, the 127/8 guard, isolation between networks, internal networks, routing closed to all but containers | `shipyard-network/nft/` (`ruleset.rs`, `expr.rs`, `objects`, `decode.rs`) | `nft/ruleset.rs`'s 10 tests, `nft/linux_tests.rs`, `tests/firewall.rs` (9) |
| `-p`/`-P`: parsing, ports held by bound sockets, kernel-chosen ports printed | `shipyard-network/ports.rs` | `the_forms_of_p`, `what_p_refuses`, `a_held_port_is_taken_and_a_free_one_is_chosen`, `a_published_port_is_held_while_its_run_lives`; the `ports` fuzz target |
| conntrack: a freed address's connections flushed | `shipyard-network/conntrack.rs` | `a_flush_forgets_an_addresses_connections`, `a_killed_runs_ports_and_connections_are_swept_by_the_next` |
| Rootless with pasta, attached by PID (by a namespace file since stage 8.1c; `tests/pasta.rs` has 10 tests since) | `shipyard-network/pasta.rs`, `pidfd.rs` | `tests/pasta.rs` (8), `nothing_published_means_no_port_forwarded_either_way` |
| A DNS server per network: names, aliases, run IDs; A, AAAA, PTR; forwarding; across networks | `shipyard-network/dns/`, the CLI's `dns_helper.rs` | `dns/*.rs`'s 25 tests, `tests/dns.rs` (7); the `dns` fuzz target |
| Several networks per container | the CLI's `network.rs` (`take_all`, `route_network`, `resolver`), `run.rs` (`parse_long_network`, `combine_networks`) | `tests/networks.rs` (7), `a_second_network_is_another_interface_without_the_default_route` |
| IPv6: ULA prefixes, NAT66, published ports on both families, router advertisements kept | `store.rs`, `attach.rs`, `nft/ruleset.rs`, `ports.rs` | `tests/ipv6.rs` (6), `only_the_hosts_own_interfaces_keep_taking_router_advertisements`, `ipv6_is_dnated_and_masqueraded_through_its_own_maps` |
| Reading a container's sockets (`/proc/<pid>/net`) | `shipyard-network/observe.rs` | `observe.rs`'s 9 tests, `a_containers_sockets_are_read_through_its_init`; the `proc_net` fuzz target |
| Commands: `network create / ls / inspect / rm / prune`; `run --network`, `--ip`, `--ip6`, `--mac-address`, `--name`, `--network-alias`, `-p`, `-P`, `--add-host`, `--dns*` | the CLI's `main.rs`, `run.rs`, `network.rs` | `a_network_is_created_listed_and_removed`, the end-to-end files above |

**Moved out of stage 7:**

| Item | Now in | Why |
|---|---|---|
| `network connect` / `disconnect` | stage 9 | they act on a running container by name, which stage 9's state gives |
| A command for a container's sockets (`port`, the UI's observed dependencies) | stage 9 | the reader is built (`observe.rs`); the command needs named containers |
| The DNS servers' own cgroup, and a restart if one is killed | stage 9 | it belongs with the engine's state and restart policies |
| A warning when a container's pasta dies | stage 9 | the supervisor reaps it quietly; events come with stage 9 |
| `exec` joining a container's network namespace | stage 8 | `Join::standard` has no network yet; `exec` is stage 8's |
| Rootless user-defined networks | later | they need a long-lived holder of a rootless network namespace (Podman's pause process) |
| `slirp4netns` | not planned | pasta covers rootless networking with better performance |

## Architecture

```
shipyard run (CLI, single-threaded at every fork)            (shipyard-cli/run.rs)
  RunArgs::check: flags, -p specs, --network modes and long forms, names
    ← no side effects
  default_network(): root that can network → bridge; root that can't → host;
                     anyone else → pasta (refused with a hint if pasta isn't installed)
  RunNetwork::set_up                                         (shipyard-cli/network.rs)
    pasta: hold the -p ports (bound, never listening) until just before the start
    bridge networks, one at a time (Attachment::take, each under its network's lock):
      ensure_firewall (table inet shipyard v4, rebuilt if missing or old)
      ensure_bridge (MAC 02:42+gateway, IPv6 settings, route_localnet)
      sweep stale leases: veth, nft elements, conntrack, lease file
      allocate the lease (round robin, O_EXCL file, flock held by the CLI)
      routing closed, then forwarding on (accept_ra 1→2 first, for IPv6)
      publish -p (route network only); a DNS server started if the network has names
    run dir /run/shipyard/runs/<id>/ {hosts, hostname, resolv.conf} (flock-held)
  Supervisor::install ─ spawn ─ clone3(… CLONE_NEWNET …)       (shipyard-runtime)
    before_go(pid), signals blocked, every step under a deadline:
      ID maps → netDevices moved → NetworkHook(NetworkTarget { pid, netns }):
        bridge: veth per network, the peer born inside as eth{n}; inside, through a
                socket made there: address(es), routes, MTU, up; accept_ra/autoconf/
                accept_dad off
        pasta:  pasta … PID (it joins the user and network namespaces); pdeathsig;
                ready when its pid file appears. (Since 8.1c: a launcher joins
                the user namespace and bind-mounts the network namespace on a
                file, which pasta gets with --netns.)
    init: lo up; its own sysfs when its user namespace owns its network namespace;
          net.* sysctls (ip_forward=0, IPv6 forwarding=0, ping range, port 0 start)
  the run ends: nft elements, veth, conntrack, lease, port sockets, pasta, run dir
                (all before the exit: die_from runs no destructors)
  the CLI SIGKILLed: every flock frees → the next run (or `network prune`) sweeps

__shipyard-dns <network>  (one per network with names; outside any run's subreaper)
  started by the first run that needs it, holding dns.lock (an inherited flock)
  bound to gateway:53 UDP+TCP, SO_BINDTODEVICE the bridge; then nobody, no_new_privs
  answers the network's containers from live leases (and those of their other
  networks); forwards everything else to the host's resolvers
  exits when idle (no live lease for 30 s, re-checked under the network lock) or
  when its bridge is gone
```

**State** (root's only; every file the DNS servers read is root's and `nogroup`'s,
0640, its directories 0750: a user who could open a lease could lock it and keep it
live):

```
/var/lib/shipyard/networks/                 (SHIPYARD_NETWORK_DIR overrides it)
  .lock                                      creates, removals and firewall rebuilds take turns
  .routing-closed[-ipv6]                     Shipyard turned forwarding on: the boot ID,
                                             then the host's interfaces at the time
  <name>/config.json                         subnet(s), gateway(s), bridge, internal
  <name>/lock                                exclusive for allocate, sweep, rm
  <name>/dns.lock, dns.log                   held by the DNS server (its bridge's index)
  <name>/leases/<ip>                         one per container, flock-held by its CLI;
                                             a reboot frees every lock, so all are swept
/run/shipyard/runs/<id>/                     a run's /etc files (rootless: under
                                             $XDG_RUNTIME_DIR/shipyard/runs/)
```

**The crates.** `shipyard-network` is the engine's network layer, the netavark-equivalent:
the portable parts (messages, the ruleset's encoding, specs, the DNS codec, the socket
table parser) build and are tested on macOS too; sending them needs Linux. The runtime
gains only the network namespace itself and a hook (`NetworkHook`) called in `before_go`;
configuring the namespace is the engine's job, as Podman and netavark split it.

**The firewall** (`nft/ruleset.rs`): one table, `inet shipyard`, version 4 (in the table's
userdata). Its rules are static; what changes per network or container is in sets and
maps, so rules are never edited:

| Set or map | Holds |
|---|---|
| `bridges`, `same_bridge`, `internal` | Shipyard's bridges; internal networks' bridges |
| `masquerade4`, `masquerade6` | subnets with a way out (`masquerade4` was `masquerade` until stage 8: a keyword of nft's language, which kept `nft -f` from reading the table back) |
| `ports`, `ports_addr`, `ports6`, `ports6_addr` | `-p` maps: protocol and port (and host address) → container address and port |
| `closed`, `uplinks` | the families whose forwarding Shipyard turned on, and the host's own interfaces then: routing between those stays closed |

| Chain | Hook | Job |
|---|---|---|
| `guard` | prerouting, filter, −300 | drop anything for or from 127/8 that didn't come in on `lo` (CVE-2020-8558) |
| `dstnat`, `output` | prerouting / output, nat | `fib daddr type local` → DNAT through the port maps (never IPv6 `::1`) |
| `forward` | forward, filter | established in; same bridge in; internal networks closed; only DNAT'd traffic reaches a bridge; nothing between two `uplinks` where routing is closed |
| `srcnat` | postrouting, nat | the way out (masquerade), hairpin, localhost clients |

## Decisions and why

**Made with the user, before building (2026-10-08):**

- **Netlink is Shipyard's own,** for rtnetlink, nf_tables and conntrack: no `ip` or `nft`
  binaries, no netlink crates. In the spirit of the seccomp and eBPF compilers: bounded,
  fuzzable, and it teaches netlink(7).
- **Rootless is pasta, one per container,** for outbound traffic, `-p` and the host's DNS.
  Rootless user-defined networks are refused with a clear message: they need a long-lived
  holder of a rootless network namespace, which comes later.
- **Defaults are Docker's:** root on `bridge` (`shipyard0`, NAT); rootless on pasta;
  rootless without pasta refused with a hint (install `passt`, or `--network host|none`),
  never silently given the host's network; OCI bundles keep the spec's meaning (no
  `network` namespace means the host's).
- **`--name` and `--network-alias` come now, for DNS only.** They're recorded in the
  network's leases; stage 9 gives `--name` the rest of its meaning.
- **`-p 8080:80` publishes on every address,** as Docker; `-p 127.0.0.1:8080:80` limits it.
- **A port the kernel chose** (`-p 80`, `-P`) is printed on stderr.
- **The default hostname is the run ID** (12 hex), for every isolated run; `--hostname`
  wins. Generated `/etc` files go into image runs always, and into a `--rootfs` only where
  the file already exists (Shipyard never writes into the user's directory).
- **What a container answers to** on a user-defined network: its `--name`, each
  `--network-alias`, and its run ID. The default `bridge` has no names, as Docker's.
- **Several networks:** `--network` repeated, plus Docker's long form
  (`name=,ip=,ip6=,mac-address=,alias=`).
- **IPv6:** off by default (`network create --ipv6`); a ULA `/64` with NAT66; Shipyard sets
  `accept_ra=2` itself on the host's interfaces that take router advertisements, before
  turning IPv6 forwarding on.
- **Sockets:** a library now (`observe.rs`), TCP and UDP, IPv4 and IPv6; the command comes
  with stage 9.
- **The milestone:** v0.1 is `run --image alpine sh` on the caller's terminal; `-it` and a
  positional image come with stages 8 and 9.

**Made while building, from what the kernel or a review showed:**

- **The bridge's MAC is set** (`02:42` + the gateway's address). An unset one takes its
  ports' lowest and changes when that port leaves: after a container left, the others'
  ARP entries for the gateway were wrong for seconds. A DNS test caught it.
- **A new namespace inherits the host's `ip_forward`.** A container on two networks would
  have routed between them. Every container with its own network gets `ip_forward=0` and
  `net.ipv6.conf.all.forwarding=0` as defaults (a user's `--sysctl` wins).
- **Turning forwarding on makes the host a router for anyone.** So routing is closed to all
  but containers (the `closed` set), *before* forwarding goes on, and the marker that says
  Shipyard did it carries the boot ID (a stale one counts for nothing after a reboot).
- **IPv6 forwarding turns router advertisements off.** Interfaces with `accept_ra=1`
  (including `default`, for interfaces made later; not Shipyard's own) are set to 2 first.
  Bridges get `accept_ra=0`, containers `accept_ra=0` and `autoconf=0`, IPv4-only networks
  `disable_ipv6=1`: a container's router advertisements are never taken (the review's
  critical finding).
- **Duplicate address detection is off on bridges and containers** (`accept_dad=0`). A new
  bridge's link-local address is tentative for a second or two, and the host solicits a
  container from it: the first connections to a published IPv6 port timed out.
- **Ports are held by bound sockets that never listen.** No other program (or run) can take
  a published port, and the DNAT happens before any socket lookup, so they never see
  traffic. An every-address port is held on `[::]` too.
- **The DNS server starts before the run's supervisor** (a subreaper would kill it at the
  run's end), as `nobody`, with only its two descriptors and two environment variables,
  and answers only its own network's containers' addresses (no reflection out of a
  network, even an internal one).
- **Locks are taken one network at a time,** never two at once, so runs joining `a, b` and
  `b, a` can't deadlock.
- **pasta was attached by PID until stage 8.1c** (it joined the user and network
  namespaces itself: no bind mount, which a rootless CLI couldn't make in the host's mount
  namespace). *Changed in 8.1c:* under
  SELinux, Fedora's `pasta_t` may not open an unconfined process's `/proc/<pid>/ns`, so a
  launcher (Shipyard re-executed) joins the container's user namespace, where it may make
  a private mount namespace and bind-mount the network namespace on a file, and execs
  pasta with `--netns FILE`, as Podman does, staying as its parent (under SELinux pasta's
  exec clears its parent-death signal).
- pasta runs in a process group of its own (Ctrl-C at the
  terminal doesn't kill it), with pdeathsig and an explicit kill at the end (bookworm's
  pasta never quits by itself).
- **The test image moved to Debian trixie:** bookworm's 2023 pasta drops connections to
  published ports on its tap path, and backports has nothing newer.
- **Sockets are read through a pidfd and one directory descriptor:** a reused PID can't mix
  two namespaces' tables, and a dying init reads as gone, not as no sockets.

## Kernel checks

`scripts/probes/network.c`, run in the privileged test container on Linux 7.0 (OrbStack):

| | Question | Result |
|---|---|---|
| N1 | Can a user namespace that owns its network namespace mount sysfs? | yes; `/sys/class/net` lists only `lo` |
| N2 | Can it write `ip_unprivileged_port_start` and `ping_group_range`? | yes, but `ping_group_range "0 2147483647"` is `EINVAL` when the groups aren't all mapped: the range must stop at the highest mapped GID |
| N3 | Is a veth peer born in another namespace with its final name? | yes (`IFLA_NET_NS_FD`), invisible from the host |
| N4 | Does a netlink socket made inside, used after `setns` back, still act inside? | yes |
| N5 | inet nat chains, masquerade, `fib daddr type local`, `ct status` | all load |
| N6 | `route_localnet` per interface | present |
| N7 | ctnetlink dumps | ok |
| N8 | How long until a veth's host end goes after its namespace's last process? | 1–43 ms, asynchronously: so it's deleted explicitly |
| N9 | Extended ACK text | "Attribute failed policy validation" (an over-long name), "Unknown device type" |

`scripts/probes/pasta.sh`, as a normal user, attached by PID:

| | bookworm `0.0~git20230309` | trixie `0.0~git20250503` |
|---|---|---|
| `--config-net`, `--dns-forward`, `--no-map-gw`, `--pid`, `-f` | yes | yes |
| `--map-guest-addr`, `--map-host-loopback` | no | yes |
| quits when the container is gone | **no** | yes |
| publishes a listening port by default | no | **yes** |
| the container reaches host loopback services by default | **yes** | **yes** |

So pasta always gets `-t none -u none -T none -U none --no-map-gw` and the ports asked for,
and is killed explicitly.

## macOS and Linux differences

- `shipyard-network` is `cfg(unix)`. Netlink messages, the nf_tables encoder and decoder,
  port specs, the store's rules, the DNS codec and answers, `/etc` file generation and the
  socket table parser build and are tested on macOS; sockets, namespaces, pasta and the
  DNS server are Linux-only (`#[cfg(target_os = "linux")]`).
- Every network flag (`--network`, `-p`, `--name`, `--dns`, …) and `shipyard network` are
  refused on macOS by name; the VM (stage 12) runs them.
- macOS's libc has no netlink constants, so Shipyard defines its own, and a Linux test
  checks them against libc's (`they_are_the_kernels`).

## Failure-mode register

| Case | Handling | Test |
|---|---|---|
| A run SIGKILLed: its lease, veth, nft elements, conntrack entries and pasta left behind | leases are `flock`-held; the next allocation (or `prune`) sweeps the dead ones with everything they name; pasta has pdeathsig | `a_killed_runs_address_is_swept_by_the_next`, `a_killed_runs_ports_and_connections_are_swept_by_the_next`, `a_killed_runs_leases_are_swept_from_each_network`, `pasta_goes_with_a_killed_cli` |
| An address reused by a new container gets a dead run's flows or ports | sweep before allocation, conntrack flushed, round-robin allocation | `a_flush_forgets_an_addresses_connections`, `addresses_go_round_the_subnet` |
| A run killed mid-allocate leaves an empty lease file | unreadable lease files nobody holds are swept, not fatal | `a_lease_left_unreadable_is_swept_not_fatal` |
| Two `network create`s choosing the same subnet | creates take turns under the store's lock | `creates_at_once_choose_different_subnets` |
| A lock file made again while a run waits | the lock is checked against its path's inode, retried | `a_lock_file_made_again_is_the_one_locked` |
| A published `127.0.0.1` port reachable from the LAN, or a container using a spoofed 127/8 source (`route_localnet`) | the `guard` chain: 127/8 only from `lo`, both directions | `the_guard_drops_what_a_bridge_sends_to_loopback` (a packet from another interface stays local in a test namespace, so the rule is pinned by a unit test) |
| `ip_forward=1` making the host a router for everyone | routing `closed` to all but containers, before forwarding goes on; boot-ID markers | `a_host_that_didnt_route_routes_only_for_its_containers`, `ipv6_routing_stays_closed_to_all_but_containers`, `routing_is_closed_per_family_and_for_this_boot_only` |
| A container routing between its two networks (`ip_forward` inherited) | `ip_forward=0` and IPv6 forwarding off by default inside | `a_container_on_two_networks_reaches_and_names_each_ones_containers` |
| A container's router advertisements taken by the host or other containers | bridges and containers `accept_ra=0`; IPv4-only networks without IPv6 | `the_way_out_is_the_hosts_ipv6_address_and_router_advertisements_still_count`, `only_the_hosts_own_interfaces_keep_taking_router_advertisements` |
| The host losing its IPv6 routes when forwarding goes on | `accept_ra` 1 → 2 on the host's interfaces, `default` included | the same two tests |
| The first IPv6 connections timing out (DAD on a fresh bridge) | `accept_dad=0` on the bridge and inside | `ports_are_published_on_both_families` |
| A client of `[::1]:port` DNAT'd into a hang | IPv6 DNAT never for `::1`; `-p [::1]:…` refused with why | `ports_are_published_on_both_families`, `what_p_refuses` |
| The bridge's MAC changing when a port leaves (stale ARP) | the MAC set from the gateway | `containers_find_each_other_by_name_alias_and_run_id` (a query right after another container stopped) |
| A ports map entry matched by key alone; a dead run's port on another network | elements removed by key and value; `publish` reclaims a dead run's port | `a_port_a_killed_run_left_on_another_network_is_reclaimed` |
| `br_netfilter` hosts filtering traffic on one bridge | `same_bridge` accepted | `only_dnated_traffic_reaches_a_bridge_and_internal_networks_are_closed` (the module isn't loaded in the test container) |
| `nft flush ruleset` while containers run | the table is noticed missing or emptied (rule count) and rebuilt by the next run | `the_ruleset_installs_is_current_and_is_rebuilt`, `only_this_versions_whole_table_is_current` |
| An answer bigger than one ACK datagram (thousands of ports) | ports go one request per map, in chunks of 256 | `networks_and_ports_come_and_go` |
| Another firewall's forward chain with a drop policy (Docker, ufw, firewalld) | detected and warned about with the exact fix; never changed | `another_firewalls_drop_policies_are_warned_about_with_their_fixes` (completed in 8.6: `dockers_firewall_is_warned_about_and_its_printed_fix_works`, `firewalld_is_warned_about_until_the_bridge_is_trusted`) |
| A spoofed DNS client (reflection out of a network) | only the network's containers' addresses are answered | `only_the_networks_containers_are_answered` |
| A DNS pointer loop | each pointer must point before the last jump's target | `pointers_that_loop_are_refused`; the `dns` fuzz target |
| One container hogging the DNS server | per-client caps (16 of 64 forwards, 8 of 64 TCP connections), a 5 s TCP frame deadline, a 4 s forward | `places_run_out_in_all_and_for_one_client`, `a_tcp_frame_must_come_whole_and_in_time` |
| Two DNS servers for one network | `dns.lock` probed under the network lock; a shared-lock look (`inspect`) told apart from a server | `one_server_runs_as_nobody_until_its_network_is_idle` |
| A bridge deleted by hand and made again (the server bound to the old one) | the server records its bridge's index in `dns.lock`; a run waits for an old one to go | `a_bridge_made_again_gets_a_server_of_its_own`, `a_server_records_its_bridge_in_its_lock` |
| A forwarded answer for another question | an upstream's answer must match the ID, name, type and class | `an_answer_must_be_to_the_question_asked` |
| The default bridge's names leaking through another network's server | the default bridge is skipped | `the_long_form_gives_a_network_its_own_settings_and_bridge_has_no_names` |
| Runs joining networks in opposite orders deadlocking | one network lock at a time | `runs_joining_networks_in_opposite_orders_both_start` |
| Ctrl-C killing pasta | pasta in a process group of its own | the terminal tests' tester pass (`--rootfs` runs use pasta) |
| A `--rootfs` with `/etc/resolv.conf` a link (a dangling Debian one) | only regular files are bound; never followed | `a_root_filesystem_gets_the_etc_files_it_has` |
| A reused PID mixing two namespaces' socket tables | a pidfd held across the read, the tables opened through one directory descriptor | `a_containers_sockets_are_read_through_its_init` |
| A busy container's socket tables (thousands of sockets) | read streaming, bounded per line and in lines | `a_line_too_long_or_too_many_is_refused` |
| Rootless `--network bridge`, or no pasta | refused (125) with what to do | `bridge_networks_need_root`, `without_pasta_a_rootless_run_says_how_to_do_without` |
| Routing closed for everyone: a Docker, libvirt or VPN started later in the same boot lost its way out (whole-stage review) | closed only between the interfaces the host had when Shipyard turned forwarding on (the `uplinks` set, recorded with the boot ID) | `a_host_that_didnt_route_routes_only_for_its_containers` (a link made after still routes, both ways), `routing_is_closed_per_family_and_for_this_boot_only` |
| A firewall flush leaving the host's loopback reachable through `route_localnet`, and internal networks routed out (review) | `route_localnet` only on a bridge whose containers publish; internal bridges' own IPv4 `forwarding=0`, set again whenever Shipyard turns forwarding on (which resets every interface's); the docs say what a flush undoes | `published_ports_reach_the_container_from_everywhere`, `an_internal_network_has_no_way_out_and_publishes_nothing` |
| Any local user locking a lease file (kept live forever: names, addresses and ports pinned) (review) | the store's files root's and `nogroup`'s (the DNS servers'), 0640, directories 0750 | `a_lease_directory_lists_live_leases_through_its_descriptor` |
| A stray file among the networks breaking every network command (review) | only directories are networks; a corrupt config's error says how to forget it | `a_stray_file_among_the_networks_is_no_network` |
| The Docker fix accepting Shipyard's containers into Docker's own bridges (review) | the fix names the host's way out (`-o eth0`) | `firewalls::tests::docker_gets_the_whole_fix_in_docker_user` (moved in 8.6), `dockers_firewall_is_warned_about_and_its_printed_fix_works` |
| A container on an IPv4-only network and an IPv6 one with no IPv6 default route (review) | each family's default route through the first network with it and a way out | `ipv6s_route_goes_through_a_network_with_ipv6`, `each_familys_default_route_goes_through_a_network_that_has_it` |
| `network rm` racing a firewall rebuild that put its rules back (review) | rules removed under the store's lock | (by review) |
| Every pasta run ending a second late (found writing the book) | an already-reaped pasta (`ECHILD`) counts as reaped | `a_rootless_run_ends_as_soon_as_its_command_does` |
| `-P` publishing a port `-p` already does on a second host port (found writing its test) | skipped, as Docker | `publish_all_adds_the_exposed_ports_p_doesnt_publish` |
| Real `nft` unable to read Shipyard's table (would break an administrator's `nft list ruleset`) | checked | `nft_reads_shipyards_whole_table` |

**Fuzzing.** Four cargo-fuzz targets cover what this stage reads from outside: `netlink`
(messages, attributes, links, addresses, routes, nf_tables objects, conntrack entries, with
seeds from real kernel dumps), `ports` (`-p` and `ExposedPorts`), `dns` (queries and
responses, answered and read back, with layered names and IPv6) and `proc_net` (each socket
table). Their seeds are in `scripts/fuzz-seeds.py`. In the final run (`test-linux.sh --fuzz 120`,
2026-10-09, Linux 7.0, aarch64), every target ran for 120 seconds with no crash:
`netlink` 14.1 million inputs, `ports` 27.9 million, `dns` 6.6 million, `proc_net` 5.6
million (and stage 6's five, 29 million more).

## Known limitations (by design, for stage 7)

- **Rootless containers can't share a network** (no user-defined rootless networks) and
  can't reach the host's loopback services. pasta's DNS (`169.254.1.1`) is IPv4 only.
- **The host's resolvers on loopback** (systemd-resolved's `127.0.0.53`) are reachable from a
  bridge container only through the network's DNS server; on the default `bridge`, loopback
  resolvers are dropped and Docker's fallback (`8.8.8.8`) used if none is left.
- **iptables-legacy rules are invisible** to Shipyard's firewall check (documented).
- **A firewall flushed while containers run** is restored only by the next run.
- **NetworkManager or systemd-networkd taking router advertisements in userspace** on a host
  that forwards IPv6 isn't handled (`accept_ra` is the kernel's setting; documented).
- **An isolated command has no controlling terminal** of its own (`sh: can't access tty;
  job control turned off` in an interactive shell), as Docker without `-t`: a
  pseudo-terminal of the container's own is stage 8's.
- **`--ip` can briefly see a dead lease as taken** while the DNS server probes it.
- **Search domains are the host's,** copied as Docker does: `db.corp` is asked upstream first.
- **A DNS server from before an upgrade** gives no cross-network names until it idles out.
- **A network's DNS server and `-p 53` on every address** can't both be had (the kernel
  sees the two bindings conflict); publish port 53 on one address.
- **A container spoofing a neighbour's address** on an internal network could have the DNS
  server forward for it (the server knows an asker by its address only); the
  anti-spoofing bridge rule is carried forward.
- **`route_localnet` stays on a bridge** once a container on it has published a port,
  until the bridge goes; the guard chain covers it while the rules are there.
- **Routing closed between the host's interfaces** covers only those it had when Shipyard
  turned forwarding on: one plugged in later is routed.
- **Each address's connections are found with a whole conntrack dump**, under the
  network's lock: slow on a host tracking very many connections.
- **The store's files are shared with `nogroup`:** another daemon running as that group
  could read (and lock) them. A group of Shipyard's own needs a system group.

## Carried forward

- **Stage 8:** `exec` joins the network namespace (`Join::standard` lacks Network); `-it`
  with a pseudo-terminal of the container's own.
- **Stage 9:** `network connect`/`disconnect`; the sockets command (`port`, observed
  dependencies); `--name` for everything; the DNS servers' cgroup and restart; a warning
  when pasta dies; the image as the first argument of `run`.
- **Later:**
  - one helper-spawning module for the storage and DNS helpers;
  - one conntrack dump per sweep (or kernel-side filtering);
  - clearing `route_localnet` when a network's last publishing container goes;
  - from the clean-code review: `network.rs`, `run.rs` and `store.rs` split into modules;
    IPv4 fields as `Ipv4Addr`; `dns::answer` crate-private; one pidfd module across the
    crates; `network inspect`'s JSON keys made consistent (`Ports` is snake_case) before
    a release fixes them;
  - sock_diag (netlink) instead of `/proc/<pid>/net` for very busy containers;
  - pasta DNS over IPv6;
  - an anti-spoofing bridge rule (address and MAC per port);
  - an ifalias mark before adopting or deleting a bridge a user made;
  - Ubuntu's AppArmor profile for passt with `$XDG_RUNTIME_DIR` (CI's normal-user job
    exercises it; not checked by hand);
  - `announce` prints ports before a later network fails.
- **Stage 14 (hardening):** the fuzz targets on a schedule in CI.

## Tests

Not counted at the stage's end. [Stage 8's notes](08-terminals.md#tests) give the
counts per file after stage 8, which include stage 7's tests.

## Review history

The stage was built in steps, each planned, built, then reviewed twice (once for clean
code, once for what could go wrong), mutation-checked and run by hand on Linux:

| Step | What | A finding of its own reviews |
|---|---|---|
| 1 | probes; netlink core; rtnetlink | an interrupted dump flagged only on its `DONE` wasn't redone; a failed dump item left the kernel's dump running (EBUSY next) |
| 2 | network namespaces in the runtime; `--network none\|host` | a joined host namespace could get `net.*` settings and devices |
| 3 | networks, leases, bridge and veth, the `/etc` files, run IDs | an empty lease from a killed allocate bricked its network; `--rootfs` `/etc` links followed |
| 4 | nf_tables, NAT, `-p`, internal networks, conntrack; root's default flips | a `127.0.0.1` port reachable from the LAN; the host left an open router |
| 5 | rootless with pasta; rootless's default flips | Ctrl-C killed pasta; ports held across the start |
| 6a | the DNS server, `--name`, `--network-alias` | spoofed sources reflected; a re-made bridge left without a server |
| 6b | several networks per container | a container routed between its networks (`ip_forward` inherited) |
| 7a | IPv6 | containers' router advertisements accepted (critical); DAD on a fresh bridge |
| 7b | reading a container's sockets | a reused PID could mix two namespaces' tables; the size cap refused busy containers |
| 8 | the milestone test, the whole-stage review, these notes, the book, fuzzing | routing closed for programs started later; a flush exposing loopback; leases lockable by anyone; Docker's fix too wide; and (writing the book) pasta's late end |
