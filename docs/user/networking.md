# Networking

A container's network is chosen with `shipyard run --network MODE`. On Linux:

| Mode | The container's network |
|---|---|
| `bridge` | The default network: an address of its own on a bridge shared with other containers and the host, and a way out. **Root's default** (where root may change the network: not inside a container without `CAP_NET_ADMIN`, say; there the container gets the host's network, and `run` warns, unless you ask for `--network host`). |
| `NAME` | A network made with `shipyard network create NAME`. Root only. |
| `pasta` | One of its own, connected through [pasta](https://passt.top): a way out and published ports. **The default without root.** |
| `host` | The host's: its interfaces, addresses and ports. |
| `none` | One of its own, with only a loopback interface (`lo`): nothing in or out. |
| `container:NAME` | Another running container's, as it is: its interfaces, addresses, ports and hostname (see [Another container's network](#another-containers-network)). |

```sh
sudo shipyard run alpine:3.20 wget -qO- example.com      # the default network
sudo shipyard run -p 8080:80 nginx:alpine                # http://localhost:8080
sudo shipyard network create app
sudo shipyard run --network app alpine:3.20 ip addr show eth0
shipyard run -p 8080:80 nginx:alpine                     # rootless, through pasta
sudo shipyard run --network front --network back alpine:3.20 ip addr   # two networks
```

> **Status:** as root, containers on a network reach each other, the host and
> the internet, and you can publish their ports. Without root, each container
> gets its own network through pasta: a way out and published ports, but no
> networks shared with others (one can join another's: `--network container:NAME`). A network made with `--ipv6`
> gives its containers IPv6 too; pasta gives them the host's IPv6. On a Mac,
> networks are the Linux VM's (its commands run as root there: bridge networks), and
> published ports answer on the Mac's `localhost` ([macOS](macos.md)).

## Rootless containers: pasta

A container without root can't have a bridge: bridges and their virtual cables
are the host's devices, and only root may change them. It gets a network of its
own through **pasta** instead, one for each container, which you install from
your distribution's `passt` package.

- **How it works:** pasta joins the container's namespaces and gives it one
  interface with the host's own addresses and routes, IPv4 and (if the host has
  it) IPv6. Each connection the
  container makes, pasta makes again from the host, as an ordinary program of
  yours would, and passes the data across. To the rest of the world, the traffic
  is yours.
- **What it can't reach:** the host's own loopback services (`127.0.0.1` on the
  host). They're the host's alone, as with a bridge network.
- **SELinux (Fedora):** works. Fedora confines pasta, so Shipyard hands it the
  container's network as a file of its own (as Podman does), never another
  process's `/proc` entry, and keeps pasta's files out of your home directory
  (in your runtime directory, `$XDG_RUNTIME_DIR`, as a login session has).
- **IPv6:** a port published on `[::1]` is refused with pasta too.
- **DNS:** the container asks `169.254.1.1`, over IPv4 (its `/etc/resolv.conf` says so, with
  an image or `--rootfs`), and pasta passes the question to the host's resolver,
  even one only the host can reach, like systemd's `127.0.0.53`. Without a root of
  its own, the container sees the host's `/etc/resolv.conf`; if that names only
  the host's own resolver (`127.0.0.53`), lookups can't work, and `shipyard` warns:
  give it an image or `--rootfs`, or `--network host`.
- **Signals:** pasta is out of the terminal's reach (Ctrl-C is the container's),
  and goes after the container. If pasta itself fails while a container runs
  (killed, or it crashes), the container runs on with only its loopback, as with
  Podman: that's said. `ps` shows `Up 3 minutes (no network)`, `inspect --shipyard`
  gives `state.network_lost` (when, and how pasta ended), the events journal has a
  `network_lost` event, and a foreground run warns when it ends. Starting the
  container again gives it a new pasta. A run that isn't a container (`--no-image`,
  say) isn't watched so.
- **Published ports** (`-p`, `-P`): pasta listens on the host's ports and carries
  connections in, on IPv4 and IPv6. As for any program of yours, ports below 1024
  need the host's
  permission (`net.ipv4.ip_unprivileged_port_start`). A port named with `-p` that's
  in use is refused before the container starts. (The port is checked, then left
  for pasta to take, so in the moment between, another program could take it:
  then pasta's error says so.)
- **Without pasta**, a rootless run is refused, saying how to do without:
  `--network host` (the host's network) or `--network none`.
- **pasta's version:** Debian 12's pasta (from 2023) can't carry connections to a
  published port into a container; use a newer one (Debian 13, Ubuntu 24.04,
  Fedora).
- `--network pasta` asks for it explicitly, root too (not under SELinux: Fedora's
  policy gives pasta no root capabilities; root's containers use bridge networks
  there). pasta goes when the run
  ends, even if `shipyard` is killed outright.

## Networks

`docker network` works against Shipyard's [API](api.md) too (root's: bridges are the
host's). A network made there can have labels and Docker's bridge options at their
defaults, kept beside it in `labels.json` and shown by `docker network inspect`;
Shipyard's own `--isolate-host` and `--conntrack-limit` are the options
`com.shipyard.network.isolate-host=true` and `com.shipyard.network.conntrack-limit=true`.

A network is a **bridge** on the host (a switch in the kernel) with a subnet of
its own. The bridge has an address on it, the **gateway** (the subnet's first by
default), so the host is on the network too. Each container on it gets a **veth
pair**, a virtual cable: one end is a port on the bridge, the other is the
container's `eth0` (on its first network: see [Several networks](#several-networks)).

```text
host                                     container
shipyard0  10.88.0.1/16 (the gateway)
  └─ shy1a2b3c4d ══════ veth ════════════ eth0  10.88.0.2/16
                                          default route via 10.88.0.1
```

- **`bridge`**, the default network, is made the first time a container uses it:
  the bridge `shipyard0`, on `10.88.0.0/16` (or the next free `/16` up to
  `10.99.0.0/16`, if a route on the host already covers it).
- **A network you make** gets the bridge `shy-` followed by 10 hexadecimal digits
  (from its name), and the first free `/24` from `10.89.0.0` upward.
- **A subnet is free** when no route on the host and no other network overlaps it,
  so a network never hides your LAN or a VPN.

A container on a network has a network of its own, as with `--network none`:
its own `/sys/class/net`, and its own `net.*` settings (`--sysctl`).

### `shipyard network create [OPTIONS] NAME`

Makes the network and its bridge, and prints its name.

| Option | Effect |
|---|---|
| `--subnet CIDR` | Its subnet, such as `10.90.0.0/24`. It mustn't overlap a host route or another network. A free `/24` by default. |
| `--gateway IP` | The bridge's address: in the subnet, neither its first (network) nor last (broadcast) address. The subnet's first host address by default. |
| `--ipv6` | IPv6 too (see [IPv6](#ipv6)). |
| `--subnet PREFIX`, `--gateway IP6` | Given a second time with IPv6, the network's IPv6 prefix (`fd00:1::/64`) and gateway; either implies `--ipv6`. The prefix needs at least as many addresses as the IPv4 subnet (a `/112` or larger). |
| `--internal` | No way in or out of the network: its containers reach each other and the host only, and publish no ports. |
| `--isolate-host` | Its containers can't reach the host itself (its services, on any of its addresses) nor the cloud's metadata addresses, but for the network's DNS server (see [What gets in](#the-way-out-and-what-gets-in)). Not for the default network, `bridge`, which has no DNS server. |
| `--conntrack-limit` | Each container address on it may have at most 8,192 connections open of those it opened; the next is refused (see [What gets in](#the-way-out-and-what-gets-in)). |
| `-l`, `--label KEY[=VALUE]` | A label on it (repeatable), kept beside it, as the API keeps them. |
| `-d`, `--driver bridge` | The only driver. |
| `-o`, `--opt KEY=VALUE` | A driver option, as the API's create takes it: Docker's bridge options at their defaults, and Shipyard's own (`com.shipyard.network.isolate-host=true` is `--isolate-host`, `com.shipyard.network.conntrack-limit=true` is `--conntrack-limit`); any other is refused (see [Through Docker's API](api.md)). |

A name is letters, digits, `_`, `.` and `-`, starting with a letter or digit, at
most 64 characters. `bridge` is the default network's.

### `shipyard network ls`

```text
NETWORK ID     NAME      DRIVER    SCOPE
5c1d0e7a93f2   app       bridge    local
8e0b4f61a2c7   bridge    bridge    local
1a2b3c4d5e6f   host      host      local
9f8e7d6c5b4a   none      null      local
```

As Docker's `network ls`, by name, the built-in `host` and `none` beside the
networks made (rootless: `host`, `none` and `pasta`, the only ones it has). `-q`
the IDs, `--no-trunc` the whole ones; `-f` Docker's `name`, `id`, `label`,
`driver`, `type` (`builtin` or `custom`), `dangling` (`true`: no container up on
it, never a built-in, as `network rm` and `network prune` count it; refused while a
container's record can't be read); `--format` its fields (`ID`, `Name`,
`Driver`, `Scope`, `IPv4`, `IPv6`, `Internal`, `Labels`, `.Label "KEY"`,
`CreatedAt`), as [`ps`'s](containers.md#formats). Subnets, gateways and the
containers on each are `network inspect`'s.

### `shipyard network inspect NETWORK...`

The networks in Docker's shape (`docker network inspect`'s: `Name`, `Id`, `Driver`,
`IPAM` with the subnets and gateways, `Internal`, `Options`, `Labels`, and the
`Containers` on it now with their addresses), as a JSON array; `-f` a Go template
over each (`-f '{{.Name}} {{.Driver}}'`).

`network inspect --shipyard` shows Shipyard's own view instead: the name, subnet,
gateway, IPv6 prefix and gateway
(`Subnet6`, `Gateway6`), bridge, whether it's internal,
when it was made, whether its DNS server is running (`DNSRunning`, see
[Names](#names)), and each running container on it: its full ID (`Container`,
what every other command takes; a run with no record, such as `--no-image`, has
none), its run ID, `--name` and aliases, hostname, interface (`eth0`, …), address
and IPv6 address, MAC address, host-side veth and published ports, each with its
`host_ip` if it has one, `host_port`, `container_port` and `protocol`.

### `shipyard network rm NETWORK...`

Removes networks and their bridges. A network a container that's up is on
(running, paused, or about to restart) is refused ("the network app is in use by
web, db": up to five, by name; a run with no record, such as `--no-image`, by its
run ID): stop it, disconnect it (`shipyard network disconnect`), or remove it. A
stopped container doesn't keep its networks, as Docker's don't: the network goes,
and the container's next start fails ("there's no network named app") until it's
connected elsewhere or removed. `bridge` is refused too. It
carries on past a network it can't remove, and exits with 1 if any failed.

### `shipyard network prune [-f]`

Removes every network no container that's up is on, except `bridge`. `--filter`
takes `until` (made before; a time, or a duration such as `24h`), `label` and
`label!`.
It asks first on a terminal; elsewhere it needs `-f`.

## Addresses

Each container on a network gets the next free address after the last one handed
out (round robin), skipping the subnet's network and broadcast addresses and the
gateway. An address freed by one container isn't handed straight to the next, so
a cached ARP entry on another container doesn't point at the wrong one.

- **`--ip IP`** asks for a particular address: it must be in the network's subnet
  and free.
- **The MAC address** is `02:42:` followed by the address's four bytes (as
  Docker's), so an address always comes with the same MAC. **`--mac-address MAC`**
  sets another: a locally administered unicast address (the first byte's two
  lowest bits `10`, such as `02:…`).

Both need `--network bridge` or a network's name, and only one: on several
networks, each network's are given in [the long form](#several-networks). So does
`--ip6` (see [IPv6](#ipv6)).

**A container's address is its own while it runs.** When it ends, the address
is freed, with its veth, its published ports, and the connections the kernel
remembers for it (so a container given the address later never receives a dead
one's traffic). If `shipyard` is killed outright (`kill -9`), the next run on
that network (or `network rm`/`prune`) finds the address no longer held and
frees it all the same.

## Names

On a network you make (`shipyard network create`), containers find each other
**by name**. The network has a DNS server on its gateway, which its containers'
`/etc/resolv.conf` names. It answers:

- **A container's names**, with its address: its `--name`, each
  `--network-alias`, its short ID (the 12 characters `ps` shows, which is also its
  hostname unless you give it `--hostname`, so `ping $(hostname)` from another
  container works) and its run ID
  (each start's own, as `network inspect` shows it). Any case works (`DB` is
  `db`). A container's
  name can change while it runs (`shipyard rename`, see
  [Containers](containers.md)): it answers to the new one within a second.
- **Its address's name** (a reverse lookup, `PTR`): its `--name`, else its short
  ID, else (a run with no record, such as `--no-image`) its run ID.
- **Everything else** goes to the host's own resolvers (those in the host's
  `/etc/resolv.conf`, even a local one such as systemd-resolved's `127.0.0.53`),
  and their answer comes back as it is. **On an internal network**, nothing goes
  out: other names are refused.

```sh
sudo shipyard network create app
sudo shipyard run -d --network app --name db postgres:16
sudo shipyard run --network app alpine:3.20 nslookup db
```

| Option | Effect |
|---|---|
| `--name NAME` | The container's name. Letters, digits, `_` and `-`, starting with a letter or digit, at most 63 characters. |
| `--network-alias NAME` | Another name it answers to (repeatable). Needs `--network` with a network you made. |

- **A name is one running container's**, on its network: a second `--name db`
  on `app` is refused while the first runs ("the name db is taken on the network
  app, by a running container"). The same name on another network is fine. Once a
  container ends (even killed outright), its names are no longer the network's
  at once: they're asked of the host's resolvers, as any other name.
- **Answers aren't cached** (their time to live is 0): containers come and go.
- **The default network `bridge` has no names** (as Docker's): its containers
  keep the host's resolvers. `--name` is accepted there, recorded (`network
  inspect` shows it), and one running container's, as on any network.
- **`--dns` names other resolvers**, instead of the network's: then the
  container's lookups of names on the network fail.

**The server** is started by the first container on the network, and stops once
the network has had no container for 30 seconds, or is removed. It binds the
gateway's port 53 (UDP and TCP) on the network's bridge only, then runs as
`nobody`, with no privileges at all. It answers only the network's containers'
own addresses. Over TCP, it serves 64 connections at once (8 from one container), and
closes each 30 seconds after it was opened, however busy: a resolver opens another.
If another DNS server already holds port 53 on every address of
the host, the container doesn't start: "starting the network app's DNS server:
binding 10.89.0.1:53 on shy-… (is another DNS server listening on every address,
port 53?): Address already in use". That includes a container that publishes
port 53 on every address (`-p 53:53/udp`): the two can't both be had, so publish
it on one address (`-p 192.0.2.10:53:53/udp`). What it had to say is in its
log, next to the network's other files
(`/var/lib/shipyard/networks/NAME/dns.log`). The networks' files there are
readable by root and the servers' group (`nogroup`, 65534) only.

The server runs in a cgroup of its own, `shipyard/dns/NAME-…` (at most 64 MiB of
memory and 136 tasks), not in the session of the command that started it: a logout,
or the service manager stopping that command's unit, leaves it running. It's two
processes. One serves, and the other, its watchdog, starts it again if it's killed
or fails: after 100 ms, then waiting twice as long each time it ends again (up to 30
seconds; afresh once it has served for a minute). Each restart is in its log ("its
worker was killed (SIGKILL): started again in 100 ms"). After 20 ends in a row that
quick, the watchdog gives up, and the next `shipyard run` on the network starts a
server afresh (saying why, if it can't). If the watchdog itself is killed, its worker
ends with it, and the network's running containers have no DNS until the next
`shipyard run` on that network starts another server. `network rm` removes the
server's cgroup with the network. If the cgroup can't be had (no cgroup v2), the
server runs where it was started, as its log says.

## Several networks

A container joins several bridge networks with `--network` once for each:

```sh
sudo shipyard network create front
sudo shipyard network create back
sudo shipyard run -d --network front --name web nginx:alpine
sudo shipyard run -d --network back --name db postgres:16
sudo shipyard run --network front --network back --name app alpine:3.20 \
    sh -c 'wget -qO- web; nc -z db 5432'
```

- **An interface on each, in order:** `eth0` on the first network, `eth1` on the
  second, and so on, each with its address there.
- **One default route**, through the first network that isn't internal (an
  internal one leads nowhere), else the first; for IPv6, through the first such
  network that has IPv6. Each network's own addresses are reached through its
  interface.
- **Published ports** (`-p`, `-P`) go to the container's address on the network
  its (IPv4) default route goes through. On internal networks only, they're refused.
- **Names:** the container's DNS server is the one of the first network that has
  names and a way out (else the first that has names). It answers the names on
  its network, and on every other network with names the asking container is on
  too: here, `app` finds both `web` and `db`, each with its address on its own
  network. `web` and `db` share no network: neither finds, nor reaches, the
  other. A server for an internal network forwards other names for a container
  with a way out on another network (it could look them up anyway).
- **The container routes nothing between its networks**
  (`net.ipv4.ip_forward=0` in it, as in every container with a network of its
  own; `--sysctl net.ipv4.ip_forward=1` for one that should).
- **`/etc/hosts`** puts the hostname on the address of the network the route goes
  through.
- **`host`, `none` and `pasta`** are alone: they can't be combined with other
  networks. A network can't be given twice.

**The long form** gives one network its own settings, as Docker's:

```sh
sudo shipyard run --network name=front,ip=10.89.0.50,alias=www \
    --network name=back,mac-address=02:42:0a:59:01:32,alias=cache ...
```

| Setting | Effect |
|---|---|
| `name=NETWORK` | The network (required). |
| `ip=IP` | The container's address there. |
| `ip6=IP` | Its IPv6 address there (a network made with `--ipv6`). |
| `mac-address=MAC` | Its hardware address there. |
| `alias=NAME` | Another name it answers to there (repeatable; a network you made only). |

`--ip`, `--ip6` and `--mac-address` are for one network alone ("with several,
give each its own"). `--network-alias` goes to every network you made that the container
joins, with each long form's own `alias=` added there.


## Connecting a running container: `network connect`, `disconnect`

A container joins a network, or leaves one, while it runs, as with Docker:

```sh
sudo shipyard network connect back web            # web joins back, at once
sudo shipyard network connect --ip 10.89.1.20 --alias api back web
sudo shipyard network disconnect back web         # and leaves it
```

- **At once, if it's running** (or paused): it gets an interface on the network
  (`eth1`, the first number free), an address, and its names there (its name, its
  short ID, its `--network-alias`es, and any `--alias` given). Its peers there find it
  by name straight away. The address is its own until it leaves the network or ends,
  though the command has gone (its monitor holds it, as for a network it started on).
- **At its next start, if it isn't:** a created or stopped container's record changes,
  and its next start joins (or doesn't join) the network.
- **Its record keeps it**, either way: a restart (by `restart`, or its restart policy)
  joins what it's on now.
- **What its start set stays:** its default route and published ports stay on the
  network that has them, and its `/etc/hosts` and `/etc/resolv.conf` are left as they
  are. So a network connected later is reached by names through the DNS server the
  container has already: each network's server answers its containers' names on their
  other networks too. A container that started only on the default network `bridge`
  (which has no names) finds the new network's names after a restart.
- **The network that carries its way out** (its IPv4 or IPv6 default route, as it is
  in the container now, and the ports its start published) is refused without
  `--force`; with it, the container is left with no default route of that family.
  (Docker moves the default route to another network instead.)
- **`--ip` and `--ip6`** must be the network's (its subnet; IPv6 only on a network
  with IPv6), checked when you connect, a stopped container too.
- **A container's monitor holds at most 2100 things** (its networks' addresses, its
  published ports, its volumes): a running container with that many may have to be
  restarted to join one more. Joining isn't bound by the descriptor limit (`ulimit -n`)
  of the shell that made the container: its monitor raises its own, within the hard
  limit. A run that would need more is refused before anything is
  made: its published ports (two sockets per port published on every address, `-P`'s
  too) and its other holds (one per network, one per volume, a few of its own) must fit
  within 2100 and within the descriptor limit (`ulimit -n`).
- **Its only network** can't be left (with none, it would join the default network at
  its next start).
- **Root only** (bridge networks are root's): a rootless container's network is
  pasta's, and `--network host` or `none` has no bridges to join either.
- **Cut short** (killed, or the host lost power, between saying it in the
  container's record and doing it): run the same `network connect` (or `disconnect`)
  again, and it finishes, saying so.
- Events: `network connect` and `network disconnect`, with the container's ID (see
  [Events](events.md)).

## IPv6

A network made with `--ipv6` is dual-stack: each container on it has an IPv6
address beside its IPv4 one.

```sh
sudo shipyard network create --ipv6 app
sudo shipyard run --network app alpine:3.20 ip -6 addr show eth0
```

- **Its prefix** is a random unique local one (a ULA `/64`, `fd…`, as RFC 4193
  makes them), free of the host's routes and other networks; or the one
  `--subnet` gives a second time. The bridge has the prefix's first address.
- **A container's IPv6 address** goes with its IPv4 one: as far into the prefix
  as its IPv4 address is into the subnet (`10.89.0.5`, `fd…::5`). `--ip6` (or the
  long form's `ip6=`) asks for another.
- **The way out** is the host's own IPv6 address: containers' connections leave
  masqueraded behind it (NAT66), as for IPv4, and only if the host has IPv6 and a
  route out.
- **Forwarding:** Shipyard turns the host's IPv6 forwarding on
  (`net.ipv6.conf.all.forwarding=1`). A host that forwards stops taking router
  advertisements on an interface that only takes them while not forwarding
  (`accept_ra=1`), and one that learns its IPv6 routes from them would lose them.
  So Shipyard sets those interfaces to `accept_ra=2` first, and the setting for
  interfaces made later (`default`) too: the host keeps learning its routes as
  before (it says so, at the info log level). Where NetworkManager or
  systemd-networkd take router advertisements themselves (`accept_ra=0`), check
  that they still do on a host that forwards. If the host didn't forward IPv6
  before, it doesn't start forwarding it for anyone but its containers, as for
  IPv4, until it restarts.
- **Router advertisements from containers count for nothing:** Shipyard's
  bridges and the containers' interfaces never take them, so no container can
  make itself the host's IPv6 router, or its neighbours'. A network without IPv6
  has IPv6 turned off on its bridge and its containers' interfaces altogether.
- **Names** have their IPv6 address too (`AAAA`), and IPv6 addresses their name.
- **Published ports** (`-p 8080:80`) answer on the host's IPv6 addresses too, to
  the container's IPv6 address; `-p [2001:db8::1]:8080:80` on one of them only.
  The host's own clients of `[::1]:8080` are refused (a packet from `::1` can't
  reach a container); `localhost` reaches it on `127.0.0.1`.
  See [Published ports](#published-ports--p--p).
- **The default network** `bridge` is IPv4 only.
- **A container routes nothing**, on IPv6 either (`net.ipv6.conf.all.forwarding=0`
  in it).

## The way out, and what gets in

A container on a network (but an internal one) reaches the internet through
the host: its packets leave with the host's address (masquerading, as Docker
does). For that, Shipyard turns on IPv4 forwarding on the host
(`net.ipv4.ip_forward=1`), as Docker does. If the host didn't route before,
Shipyard's rules keep it that way for everything but its containers: the host
doesn't start routing between the interfaces it had then (your LAN and a VPN,
say), until it restarts. It keeps a note of that closing (in its networks'
directory, `.routing-closed`), so the rules it rebuilds (after `nft flush ruleset`,
say) close it again. If that note can't be read while forwarding is on, Shipyard
closes routing between the host's interfaces as they are now, and warns, naming the
file: if this host is meant to route, delete the file once Shipyard's forwarding is
off, or after a reboot. Interfaces that come later (Docker's or libvirt's
bridges, a VPN started after) are routed: their programs turned forwarding on for
them, and filter them themselves. An internal network's bridge doesn't forward
at all, firewall or not.

What gets in:

- **The host** reaches every container on every network, by its address.
- **Containers on one network** reach each other.
- **Containers on different networks don't**, and **nothing outside the host**
  reaches a container's address, except through a **published port**.
- **A container sends only as itself.** Root in a container can write any packet
  (it has `CAP_NET_RAW` by default), but on a network only what's from its own
  addresses and hardware address gets past its port of the bridge: it can't pass
  for another container, the gateway or the host, nor answer ARP (or IPv6's
  neighbour discovery) for an address that isn't its own. Only IPv4, ARP and IPv6
  get through at all: frames inside VLAN tags, and other protocols, don't. So a
  container can't act as a router, a VPN or a bridge for others on the network, nor
  run a DHCP client or an address probe from `0.0.0.0` (`arping -D`). Changing its
  address or hardware address inside (with `--cap-add NET_ADMIN`) cuts it off. A
  link-local IPv6 address isn't checked yet.
- **Packets the kernel's connection tracking finds invalid** (outside any
  connection's window, say) are dropped on their way into or out of a network.
- **A container reaches the host itself** (a service on any of the host's addresses,
  even one listening only on the network's gateway), and the cloud's metadata
  service where the host is a cloud machine, unless its network was made with
  **`--isolate-host`**. Then it reaches only the network's DNS server on the host
  (its gateway's port 53), and IPv6's neighbour discovery (solicitations and
  advertisements); the host's replies, and what the host publishes, still reach it.
  Nor does it reach the metadata addresses: all of 169.254.0.0/16 (the
  link-local block: most clouds' 169.254.169.254, AWS's DNS and time servers),
  Alibaba's 100.100.100.200, and `fd00:ec2::254`, `fd00:ec2::23` and
  `fd20:ce::254`. (Docker has no such option; Podman's is a firewall rule of your
  own.)
  - **Per network:** a container that's on an isolated network and on a plain one
    reaches the host through the plain one.
  - **Kept by the firewall table:** if something flushes Shipyard's `nft` table
    (`nft flush ruleset`, a firewall service's restart), isolation is off until a
    Shipyard command that touches networks runs again and puts it back.
  - **A downgrade** to a Shipyard without `--isolate-host` rebuilds the table
    without it: an isolated network's containers then reach the host. A newer
    Shipyard's table is left as it is by an older one.
- **A container can open as many connections as the host's connection table
  holds** (`net.netfilter.nf_conntrack_max`, often 65,536 to 262,144), and one that
  fills it stops every new connection on the host, the host's own included, unless
  its network was made with **`--conntrack-limit`**. Then each container address
  on it may have at most **8,192** connections open of those it opened: to the
  world, to the host, and to containers on other networks (UDP "connections" count
  too, until they time out: about 30 seconds unanswered, 2 minutes answered). A
  connection counts once, on its first packet. The next new one is refused:
  - **TCP** with a reset, so `connect` fails at once with `Connection refused`;
  - **anything else** with an ICMP "administratively prohibited", which a program
    sees as `No route to host` (`EHOSTUNREACH`) over IPv4 and `Permission denied`
    (`EACCES`) over IPv6. The kernel rate-limits ICMP errors
    (`net.ipv4.icmp_ratelimit`), so past a few refusals a second some are just
    dropped, and the program waits for its own timeout.

  What it doesn't count, and its limits:
  - **Per address, per network:** an address on each of two limited networks has
    8,192 each, and a container's IPv6 address counts apart from its IPv4 one.
  - **Within one network:** connections between containers on the same network
    are switched by its bridge, not routed, so they count only where the host
    loads `br_netfilter` (which hands bridged traffic to the firewall).
  - **Connections to it** (its published ports) count against whoever opened them.
  - **Its DNS lookups count:** each query to the network's DNS server is a UDP
    connection for about 30 seconds, so a container at its limit can't resolve
    names either.
  - **Attempts Shipyard's own rules then refuse** (an isolated network's to the
    host, say) count for about a second.
  - **An IPv6 link-local source** (`fe80::/10`) can't open connections to the host
    from a limited network: the network's bridge can't tell one container's
    made-up link-local addresses from another's, and each would have a count of its
    own. Neighbour discovery isn't affected.
  - **At most 65,536 addresses are counted at once** across the host's limited
    networks. A new address beyond that isn't limited.
  - **A small host** (about 1 GiB of memory or less) may have a table no larger
    than 8,192 to begin with: there, one container can still fill it.
  - **Kept by the firewall table:** after something flushes Shipyard's table there
    is no limit until a Shipyard command that touches networks puts it back, and a
    rebuild (a Shipyard update that changes the table) forgets the connections
    already open: they aren't counted until they end. A downgrade to a Shipyard
    without `--conntrack-limit` rebuilds the table without the limit.
  - (Docker has no such option; Podman's is a firewall rule of your own.)

### Published ports (`-p`, `-P`)

`-p` publishes a container's port on the host's:

| `-p` | Publishes |
|---|---|
| `8080:80` | the host's port 8080, on every IPv4 address (and every IPv6 one, on a network with IPv6), to the container's 80 |
| `127.0.0.1:8080:80` | the same on that address only (here: from this machine only) |
| `80` | a free host port to the container's 80 (`shipyard port` shows which) |
| `127.0.0.1::80` | a free port on that address |
| `[2001:db8::1]:8080:80` | on that IPv6 address only (in brackets; a network with IPv6) |
| `0.0.0.0:8080:80`, `[::]:8080:80` | every address, as `8080:80` |
| `53:53/udp` | UDP (TCP is the default) |
| `8000-8010:80-90` | a range: each host port to its container port |

`-P` (`--publish-all`) publishes every port the image says it uses (its
`ExposedPorts`) on free host ports, but those `-p` publishes already. A port the kernel chose (`-p 80`, `-P`) is
printed when the container starts, one line each:

```text
shipyard: published 0.0.0.0:41665 -> 80/tcp
```

- **The host's port is held** while the container runs (on IPv6 too, for a port
  published on every address, whether the network has IPv6 or not): no other program (or container) can take it, and a
  second `-p 8080:80` is refused with "port 8080/tcp on 0.0.0.0 is already in use".
  A TCP port that only closed connections still wait on (the minute or so after a
  program or container stopped serving it, `TIME_WAIT`) is free to publish: a
  container started again at once gets its port back. (Holding such a port takes a
  lock in your own runtime directory, which no other user can hold up; without one,
  an older shared lock any user can take.)
- **Published on `127.0.0.1`, it's this machine's only:** nothing that comes in on
  another interface reaches it, nor anything else on 127.0.0.0/8.
- **It works from everywhere:** other machines, the host itself (its own
  addresses and `127.0.0.1`), and other containers (even the same one) through
  the host's address. (For `127.0.0.1`, the network's bridge may route packets
  from the host's loopback, `route_localnet`, once a container on it publishes a
  port; the firewall's rules keep containers from using that to reach the host's
  own loopback services.)
- **Not with an internal network**, `--network host` (the container's ports are
  the host's already) or `none`. Not on `[::1]`: the kernel never routes the IPv6
  loopback address off the host, so a port there can't reach a container
  (`127.0.0.1` can be published on).
- **As many as its monitor can hold:** a port published on every address takes two
  of the container monitor's 2100 holds (one on a single address), `-P`'s too, beside
  one per network, one per volume and a few of its own; within the descriptor limit
  (`ulimit -n`) too. A run that would need more is refused before anything is made.

### Other firewalls

Shipyard's rules live in nftables tables of their own, `inet shipyard` and
`bridge shipyard` (each container's port: what it may send as), which it makes and
keeps up to date. On a kernel without bridge filtering (`nf_tables` for bridges),
the second can't be made: `run` warns that containers can send as one another
there, and goes on. It never changes anyone else's rules. If another
program flushes the firewall (`nft flush ruleset`, a firewall service reloading,
as `systemctl restart nftables` does), the containers that run lose, until the
next `shipyard run` rebuilds the table whole:

- their published ports and their way out;
- what keeps networks apart, and the host's routing closed to others;
- the guard that keeps them from the host's loopback services, on a network
  whose containers publish ports (a container with `CAP_NET_RAW`, which it has by
  default, could reach a service listening on `127.0.0.1` only);
- what keeps each from sending as another.

So after a flush, run any container on a network at once: `sudo shipyard run
--no-image true` (on the default network) puts the table back.

Another firewall can still drop what Shipyard lets through. A rule of
Shipyard's that accepts a packet ends only Shipyard's own chain, while a drop in
any other firewall's is final. So when a container joins a network, Shipyard
reads the other firewalls. It warns about each one in the way, says what it
would keep from the containers, and gives the fix in that firewall's own terms.
It never changes them itself. Once the fix is in, the warning stops: each rule
it printed has to be there, in the right direction. A rule for a former way out
doesn't count, nor one narrowed to a port. `shipyard doctor` shows the same thing
for the default network, on its `firewall` line.

**firewalld** (Fedora, RHEL, openSUSE) puts an interface that's in no zone in its
default zone, which rejects what it doesn't allow:

- the questions a network's containers ask its DNS server, always;
- their way out, where the zone doesn't forward. Fedora's does (firewalld 1.0
  and later); firewalld before 1.0, or a zone with `forward: no`, doesn't.

So on Fedora the default network, which has no DNS server, works as it is. A
network of your own needs its bridge in firewalld's `trusted` zone:

```sh
sudo firewall-cmd --permanent --zone=trusted --add-interface=shy-1a2b3c4d5e && sudo firewall-cmd --reload
```

The binding is kept across reboots and holds even while the bridge doesn't exist.
Each network's bridge needs its own line: `shipyard network inspect --shipyard
NAME` names it (`Bridge`), and the warning gives the line in full.

**Docker** drops forwarded traffic by default, and keeps its `DOCKER-USER` chain for
your own rules. Add three:

```sh
sudo iptables -I DOCKER-USER -i shipyard0 -o shipyard0 -j ACCEPT
sudo iptables -I DOCKER-USER -i shipyard0 -o eth0 -j ACCEPT
sudo iptables -I DOCKER-USER -o shipyard0 -m conntrack --ctstate RELATED,ESTABLISHED,DNAT -j ACCEPT
```

- The first is for containers on the network reaching each other. Docker makes
  the kernel filter bridged traffic too (`br_netfilter`); on a host where it
  doesn't, containers on one bridge never meet the chain, and the warning leaves
  this line out.
- The second is for their way out. `eth0` is the host's way out (its default
  route's interface), as the warning names it: only out to the world, never into
  Docker's own bridges.
- The third is for replies, and connections to published ports.

A network with IPv6 also needs the same three with `ip6tables`, naming IPv6's way
out; the warning gives them. Docker rewrites its own chains when it restarts, but
leaves `DOCKER-USER` alone. The rules last until a reboot unless you save them
(`iptables-save`, or your distribution's way). If the host's way out changes (a
dock, Wi-Fi instead of Ethernet), the warning comes back with the new name.

**ufw** (Ubuntu) drops forwarded traffic by default once it's enabled. Its own rules
let the containers through, and ufw keeps them:

```sh
sudo ufw route allow in on shipyard0 out on shipyard0
sudo ufw route allow in on shipyard0 out on eth0
sudo ufw route allow out on shipyard0
```

The last is for published ports: ufw lets replies through itself. A network of
your own also needs `sudo ufw allow in on BRIDGE to any port 53` for its DNS server. With Docker on the
same host, these are the rules given too: Docker's chains hand what isn't
Docker's on to ufw's, and ufw keeps its rules across reboots (Docker's three
rules in `DOCKER-USER` work as well, and count).

**Any other chain** that drops forwarded traffic by default (`policy drop`), or
ends in a rule that drops or rejects everything (RHEL's `iptables-services` ends
its `FORWARD` with `-j REJECT`), gets the same rules as Docker's, inserted in that
chain:

- with `iptables` (or `ip6tables`) for an `ip filter` (`ip6 filter`) table;
- with `nft` for any other, four lines, since nft writes a connection's state
  and its address translation apart.

A chain that drops what comes in to the host by default also drops the questions
a network's containers ask its DNS server, on the gateway. The warning gives the
rules that let in port 53 from the network's bridge.

Rules made with the older iptables (iptables-legacy rather than iptables-nft) live
outside nftables, where Shipyard can't read them: they get no warning.

## Another container's network

`--network container:NAME` starts a container in another's network: the same
interfaces, addresses and ports (a server in one is reached from the other on
`localhost`), the other's hostname, and `/etc/hosts` and `/etc/resolv.conf` made as
the other's are. Everything else (its processes, its files, its mounts) is its own.
Compose's `network_mode: "service:NAME"` is the same.

```sh
shipyard run -d --name web nginx:alpine
shipyard run --rm --network container:web alpine:3.20 wget -qO- localhost
```

- The other container must be running (or paused) at each start: `cannot join network of
  a non running container: NAME` otherwise. It's kept by its ID, so a rename changes
  nothing.
- Nothing of its own can go in that network: `--hostname`, `--add-host`, `--dns*`,
  `-p`/`-P`, `--ip`, `--mac-address`, `--network-alias`, another `--network` and `net.*`
  settings are refused, and so is `network connect`. A container that joins one that
  joins another's is refused too: join that one.
- When the other stops, restarts or goes, those that join it lose their network (it was
  the other's), and `stop`, `restart` and `rm` say so. Start them again once it's up. At
  boot, `system recover` starts them after the others.
- Without root, the other's user namespace is joined too (it owns that network), with its
  ID maps.
- `inspect` shows `HostConfig.NetworkMode` as `container:ID` and no networks of its own;
  `stats` shows no network traffic of its own.

## The hostname and `/etc` files

An isolated container with a network of its own (any `--network` but `host`)
is named by its **run ID**, 12 hexadecimal digits new for each run, as Docker
names a container. With the host's network it keeps the host's name, the one the
host's `/etc/hosts` and DNS know. `--hostname` sets another either way.

A container with a root filesystem of its own (an image or `--rootfs`) gets
three files Shipyard writes for that run, as Docker's do:

| File | What it says |
|---|---|
| `/etc/hostname` | The hostname. |
| `/etc/hosts` | `localhost` (`127.0.0.1` and `::1`), and the hostname: on the container's addresses on a network (IPv4, and IPv6 if it has it; the one its default route goes through, on several), else on `127.0.1.1`. With `--network host`, the host's `/etc/hosts`. Then each `--add-host`. |
| `/etc/resolv.conf` | The resolvers (DNS servers) the host uses, below. |

- **With `--rootfs`**, only the files DIR already has, as regular files, are
  replaced: Shipyard never writes into your directory. A symbolic link (such as
  Debian's `/etc/resolv.conf`, which points into `/run`) is left as it is. With
  an image, all three always (the image's own are hidden for the run; the image
  is unchanged).
- **A volume on one of them wins** (`--volume ./hosts:/etc/hosts`).
- **Without a root of its own**, the container sees the host's files.

### Resolvers

- **With `--network host`**, the host's `/etc/resolv.conf` as it is.
- **On a network you made**, the network's DNS server (its gateway, see
  [Names](#names)), and the host's search domains and options. On several, see
  [Several networks](#several-networks).
- **With another network of its own**, the host's resolvers without the loopback ones
  (`127.0.0.0/8`, `::1`) and IPv6 link-local ones (`fe80::…`). Those are the
  host's own servers, or on its own links, which the container can't reach from
  its network. Where the host runs systemd-resolved, its real
  upstream servers are used (`/run/systemd/resolve/resolv.conf`), not its
  `127.0.0.53` stub. If none is left, Google's `8.8.8.8` and `8.8.4.4`, as Docker
  falls back to.

These change the files:

| Option | Effect |
|---|---|
| `--dns IP` | A resolver, instead of the host's (repeatable). |
| `--dns-search DOMAIN` | A search domain, instead of the host's (repeatable). |
| `--dns-option OPTION` | A resolver option, such as `ndots:2`, instead of the host's (repeatable). |
| `--add-host NAME:IP` | An extra line in `/etc/hosts` (repeatable). IPv6 works: `db:fd00::5`. |

They need an image or `--rootfs`.

```sh
shipyard run --dns 1.1.1.1 --add-host db:10.0.0.5 alpine:3.20 \
    cat /etc/resolv.conf /etc/hosts
```

## Errors

These stop a run before the command starts (`run` exits with 125, `create` with 1):

- `--network bridge` or a name without root: "bridge networks need root for now:
  a bridge and its veth pairs are the host's devices (rootless containers get a
  network of their own through pasta, the default, or --network pasta)".
- No pasta installed, for a rootless run (or `--network pasta`): "install the passt
  package", with the alternatives.
- A network setup that fails as root by default (no right to make a bridge, say
  inside an unprivileged container): the error, with the hint that
  `--network host` runs it on the host's network.
- `-p` or `-P` with `--network host` or `none`, or on an internal network; a `-p`
  that can't be read; a host port in use.
- A network that doesn't exist: "there's no network named app (make it with
  `shipyard network create app`)".
- `--ip` outside the subnet or taken; a network with no free address.
- `-p` on `[::1]` or a link-local address (`fe80::…`), or on an IPv6 address on a
  network without IPv6.
- `network create --ipv6` on a host whose kernel has IPv6 off.
- `--ip6` on a network without IPv6, or an IPv6 address that's taken or not the
  network's.
- `--ip`, `--ip6` or `--mac-address` without a bridge network, or with several
  (give each network its own in the long form).
- `--network host`, `none` or `pasta` with other networks; a network given twice;
  a long form that can't be read (an unknown setting, a setting given twice, no
  `name=`).
- A `--name` or `--network-alias` that isn't a name, or that a running container
  on the network answers to already; `--network-alias` without a network you
  made.
- A network's DNS server that can't start (port 53 taken, say).
- `--add-host` or a `--dns` flag without an image or `--rootfs`.
- Any of these with `--no-isolation` or `--bundle`.
- A device already called `shipyard0` (or a network's bridge name) that isn't a
  bridge.

See [Running commands](run.md#network) for `none`, `host` and the network
settings (`--sysctl net.*`).
