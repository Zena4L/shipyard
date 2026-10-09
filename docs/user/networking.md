# Networking

A container's network is chosen with `shipyard run --network MODE`. On Linux:

| Mode | The container's network |
|---|---|
| `bridge` | The default network: an address of its own on a bridge shared with other containers and the host, and a way out. **Root's default** (where root may change the network: not inside a container without `CAP_NET_ADMIN`, say; there the container gets the host's network, and `run` warns, unless you ask for `--network host`). |
| `NAME` | A network made with `shipyard network create NAME`. Root only. |
| `pasta` | One of its own, connected through [pasta](https://passt.top): a way out and published ports. **The default without root.** |
| `host` | The host's: its interfaces, addresses and ports. |
| `none` | One of its own, with only a loopback interface (`lo`): nothing in or out. |

```sh
sudo shipyard run --image alpine:3.20 wget -qO- example.com      # the default network
sudo shipyard run -p 8080:80 --image nginx:alpine                # http://localhost:8080
sudo shipyard network create app
sudo shipyard run --network app --image alpine:3.20 ip addr show eth0
shipyard run -p 8080:80 --image nginx:alpine                     # rootless, through pasta
sudo shipyard run --network front --network back --image alpine:3.20 ip addr   # two networks
```

> **Status:** as root, containers on a network reach each other, the host and
> the internet, and you can publish their ports. Without root, each container
> gets its own network through pasta: a way out and published ports, but no
> networks shared with others (those come later). A network made with `--ipv6`
> gives its containers IPv6 too; pasta gives them the host's IPv6. Networking
> isn't available on macOS yet.

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
- **IPv6:** a port published on `[::1]` is refused with pasta too.
- **DNS:** the container asks `169.254.1.1`, over IPv4 (its `/etc/resolv.conf` says so, with
  an image or `--rootfs`), and pasta passes the question to the host's resolver,
  even one only the host can reach, like systemd's `127.0.0.53`. Without a root of
  its own, the container sees the host's `/etc/resolv.conf`; if that names only
  the host's own resolver (`127.0.0.53`), lookups can't work, and `shipyard` warns:
  give it `--image` or `--rootfs`, or `--network host`.
- **Signals:** pasta is out of the terminal's reach (Ctrl-C is the container's),
  and goes after the container. If pasta itself fails while the container runs,
  the container loses its network without a word (for now).
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
- `--network pasta` asks for it explicitly, root too. pasta goes when the run
  ends, even if `shipyard` is killed outright.

## Networks

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

A name is letters, digits, `_`, `.` and `-`, starting with a letter or digit, at
most 64 characters. `bridge` is the default network's.

### `shipyard network ls`

```text
NETWORK  SUBNET        SUBNET6                   GATEWAY    BRIDGE          CONTAINERS
app      10.89.0.0/24  fd3c:91a2:7d04:5e10::/64  10.89.0.1  shy-3f9a0c2e71  2
bridge   10.88.0.0/16  -                         10.88.0.1  shipyard0       0
```

`CONTAINERS` counts the running containers on each.

### `shipyard network inspect NETWORK...`

The networks as JSON: the name, subnet, gateway, IPv6 prefix and gateway
(`Subnet6`, `Gateway6`), bridge, whether it's internal,
when it was made, whether its DNS server is running (`DNSRunning`, see
[Names](#names)), and each running container on it (its run ID, `--name` and
aliases, hostname, interface (`eth0`, …), address and IPv6 address, MAC address,
host-side veth
and published ports,
each with its `host_ip` if it has one, `host_port`, `container_port` and
`protocol`).

### `shipyard network rm NETWORK...`

Removes networks and their bridges. A network a running container uses is
refused ("in use by 2 container(s)"), and so is `bridge`. It carries on past a
network it can't remove, and exits with 125 if any failed.

### `shipyard network prune [-f]`

Removes every network no running container uses, except `bridge`. It asks first
on a terminal; elsewhere it needs `-f`.

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
  `--network-alias`, and its run ID. Any case works (`DB` is `db`).
- **Its address's name** (a reverse lookup, `PTR`): its `--name`, or its run ID.
- **Everything else** goes to the host's own resolvers (those in the host's
  `/etc/resolv.conf`, even a local one such as systemd-resolved's `127.0.0.53`),
  and their answer comes back as it is. **On an internal network**, nothing goes
  out: other names are refused.

```sh
sudo shipyard network create app
sudo shipyard run --network app --name db --image postgres:16 &
sudo shipyard run --network app --image alpine:3.20 nslookup db
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
own addresses. If another DNS server already holds port 53 on every address of
the host, the container doesn't start: "starting the network app's DNS server:
binding 10.89.0.1:53 on shy-… (is another DNS server listening on every address,
port 53?): Address already in use". That includes a container that publishes
port 53 on every address (`-p 53:53/udp`): the two can't both be had, so publish
it on one address (`-p 192.0.2.10:53:53/udp`). What it had to say is in its
log, next to the network's other files
(`/var/lib/shipyard/networks/NAME/dns.log`). The networks' files there are
readable by root and the servers' group (`nogroup`, 65534) only.

The server isn't a daemon anyone looks after: if it's killed (or the service
manager stops the session of the command that started it), the network's
running containers have no DNS until the next `shipyard run` on that network
starts another.

## Several networks

A container joins several bridge networks with `--network` once for each:

```sh
sudo shipyard network create front
sudo shipyard network create back
sudo shipyard run --network front --name web --image nginx:alpine &
sudo shipyard run --network back --name db --image postgres:16 &
sudo shipyard run --network front --network back --name app --image alpine:3.20 \
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

## IPv6

A network made with `--ipv6` is dual-stack: each container on it has an IPv6
address beside its IPv4 one.

```sh
sudo shipyard network create --ipv6 app
sudo shipyard run --network app --image alpine:3.20 ip -6 addr show eth0
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
say), until it restarts. Interfaces that come later (Docker's or libvirt's
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

### Published ports (`-p`, `-P`)

`-p` publishes a container's port on the host's:

| `-p` | Publishes |
|---|---|
| `8080:80` | the host's port 8080, on every IPv4 address (and every IPv6 one, on a network with IPv6), to the container's 80 |
| `127.0.0.1:8080:80` | the same on that address only (here: from this machine only) |
| `80` | a free host port to the container's 80 (`network inspect` shows which) |
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
  So is a port a program has just stopped using, for a minute (while the kernel
  closes its connections, `TIME_WAIT`).
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
true` (on the default network) puts the table back.

Another firewall can still drop what Shipyard lets through: Docker, ufw and
firewalld make the host drop forwarded traffic by default. Shipyard finds such
a chain and warns, with the fix. For Docker:

```sh
sudo iptables -I DOCKER-USER -i shipyard0 -o eth0 -j ACCEPT
sudo iptables -I DOCKER-USER -o shipyard0 -m conntrack --ctstate RELATED,ESTABLISHED -j ACCEPT
```

where `eth0` is the host's way out (its default route's interface, which the
warning names): only out to the world, never into Docker's own bridges.

A firewall that drops what comes in to the host by default (ufw's `INPUT`
chain, say) also drops the questions a network's containers ask its DNS server,
on the gateway. Shipyard warns about such a chain too, with the fix: letting in
port 53 from the network's bridge.

Rules made with the older iptables (iptables-legacy rather than iptables-nft)
can't be seen, so they get no warning.

## The hostname and `/etc` files

An isolated container with a network of its own (any `--network` but `host`)
is named by its **run ID**, 12 hexadecimal digits new for each run, as Docker
names a container. With the host's network it keeps the host's name, the one the
host's `/etc/hosts` and DNS know. `--hostname` sets another either way.

A container with a root filesystem of its own (`--image` or `--rootfs`) gets
three files Shipyard writes for that run, as Docker's do:

| File | What it says |
|---|---|
| `/etc/hostname` | The hostname. |
| `/etc/hosts` | `localhost` (`127.0.0.1` and `::1`), and the hostname: on the container's addresses on a network (IPv4, and IPv6 if it has it; the one its default route goes through, on several), else on `127.0.1.1`. With `--network host`, the host's `/etc/hosts`. Then each `--add-host`. |
| `/etc/resolv.conf` | The resolvers (DNS servers) the host uses, below. |

- **With `--rootfs`**, only the files DIR already has, as regular files, are
  replaced: Shipyard never writes into your directory. A symbolic link (such as
  Debian's `/etc/resolv.conf`, which points into `/run`) is left as it is. With
  `--image`, all three always (the image's own are hidden for the run; the image
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

They need `--image` or `--rootfs`.

```sh
shipyard run --image alpine:3.20 --dns 1.1.1.1 --add-host db:10.0.0.5 \
    cat /etc/resolv.conf /etc/hosts
```

## Errors

These exit with 125, before the command starts:

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
- `--add-host` or a `--dns` flag without `--image` or `--rootfs`.
- Any of these with `--no-isolation` or `--bundle`.
- A device already called `shipyard0` (or a network's bridge name) that isn't a
  bridge.

See [Running commands](run.md#network) for `none`, `host` and the network
settings (`--sysctl net.*`).
