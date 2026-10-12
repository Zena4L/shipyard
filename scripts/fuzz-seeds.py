#!/usr/bin/env python3
"""Writes the fuzz targets' starting corpora into DIR/<target>/.

Each target starts from real-shaped inputs, so the fuzzer spends its time on
the edges of valid documents and archives, not on finding a first `{` or a
tar header: registry documents as Docker Hub serves them (`null` lists
included), `WWW-Authenticate` headers from real registries, references, and
small layers, the hostile ones among them (escapes through symlinks, `..`,
hard links and whiteouts), and netlink datagrams as the kernel sends them.

Usage: fuzz-seeds.py DIR
"""

import io
import json
import os
import socket
import struct
import sys
import tarfile

SHA = "sha256:" + "a" * 64


def manifest_seeds():
    layer = {
        "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
        "digest": SHA,
        "size": 3,
    }
    yield {
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {
            "mediaType": "application/vnd.oci.image.config.v1+json",
            "digest": SHA,
            "size": 2,
        },
        "layers": [layer, layer],
    }
    yield {
        "schemaVersion": 2,
        "mediaType": "application/vnd.docker.distribution.manifest.v2+json",
        "config": {
            "mediaType": "application/vnd.docker.container.image.v1+json",
            "digest": SHA,
            "size": 2,
        },
        "layers": None,
    }
    yield {
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.index.v1+json",
        "manifests": [
            {
                "mediaType": "application/vnd.oci.image.manifest.v1+json",
                "digest": SHA,
                "size": 7,
                "platform": {"architecture": arch, "os": "linux", "variant": variant},
            }
            for arch, variant in [("amd64", None), ("arm64", "v8"), ("arm", "v7")]
        ],
    }
    # hello-world's config, as Docker writes it: unset lists are null.
    yield {
        "architecture": "arm64",
        "variant": "v8",
        "os": "linux",
        "config": {
            "Env": ["PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"],
            "Entrypoint": None,
            "Cmd": ["/hello"],
            "WorkingDir": "",
            "User": "",
        },
        "rootfs": {"type": "layers", "diff_ids": [SHA]},
    }


CHALLENGES = [
    'Bearer realm="https://auth.docker.io/token",service="registry.docker.io",'
    'scope="repository:library/alpine:pull"',
    'Bearer realm="https://ghcr.io/token",service="ghcr.io",scope="repository:a/b:pull"',
    'Basic realm="Registry Realm"',
    'Bearer realm="https://a.example/t?x=1", Basic realm="r, with \\"quotes\\""',
    "Bearer realm=https://unquoted.example/token,service=svc",
    '{"token":"abc","access_token":"def","expires_in":300}',
    '{"access_token":null}',
]

REFERENCES = [
    "alpine",
    "alpine:3.20",
    "library/alpine:latest",
    "docker.io/library/alpine",
    "ghcr.io/owner/repo:tag",
    "localhost:5000/a/b/c@" + SHA,
    "registry.example.com:443/x:y@" + SHA,
    "index.docker.io/x",
]


def tar_bytes(build, fmt=tarfile.GNU_FORMAT):
    out = io.BytesIO()
    with tarfile.open(fileobj=out, mode="w", format=fmt) as tar:
        build(tar)
    return out.getvalue()


def add(tar, name, kind=tarfile.REGTYPE, data=b"", link="", mode=0o644):
    info = tarfile.TarInfo(name)
    info.type = kind
    info.mode = mode
    info.linkname = link
    info.size = len(data) if kind == tarfile.REGTYPE else 0
    tar.addfile(info, io.BytesIO(data) if kind == tarfile.REGTYPE else None)


def layer_seeds():
    def plain(tar):
        add(tar, "etc", tarfile.DIRTYPE, mode=0o755)
        add(tar, "etc/hostname", data=b"box\n")
        add(tar, "tmp", tarfile.DIRTYPE, mode=0o1777)
        add(tar, "bin/sh", tarfile.SYMTYPE, link="busybox")
        add(tar, "bin/busybox", data=b"\x7fELF", mode=0o755)
        add(tar, "bin/ls", tarfile.LNKTYPE, link="bin/busybox")

    def through_symlink(tar):
        add(tar, "etc", tarfile.SYMTYPE, link="../outside")
        add(tar, "etc/canary", data=b"dead")

    def climbing(tar):
        add(tar, "../outside/canary", data=b"dead")
        add(tar, "/abs/../../outside/x", data=b"x")

    def whiteout_through_symlink(tar):
        add(tar, "x", tarfile.SYMTYPE, link="../outside")
        add(tar, "x/.wh.canary")
        add(tar, "x/.wh..wh..opq")

    def hard_link_out(tar):
        add(tar, "h", tarfile.SYMTYPE, link="../outside")
        add(tar, "stolen", tarfile.LNKTYPE, link="h/canary")
        add(tar, "up", tarfile.LNKTYPE, link="../outside/canary")

    def special(tar):
        add(tar, "dev", tarfile.DIRTYPE, mode=0o755)
        add(tar, "dev/null", tarfile.CHRTYPE)
        add(tar, "fifo", tarfile.FIFOTYPE)
        add(tar, "d", tarfile.DIRTYPE, mode=0)
        add(tar, "d/.wh..wh..opq")
        add(tar, "d/f", data=b"replaced")
        add(tar, ".wh.d")

    def long_names(tar):
        add(tar, "a/" * 60 + "file", data=b"deep")
        add(tar, "l", tarfile.SYMTYPE, link="t" * 300)

    for name, build in [
        ("plain", plain),
        ("through-symlink", through_symlink),
        ("climbing", climbing),
        ("whiteout-through-symlink", whiteout_through_symlink),
        ("hard-link-out", hard_link_out),
        ("special", special),
        ("long-names", long_names),
    ]:
        yield name, tar_bytes(build)
    yield "pax-long-names", tar_bytes(long_names, tarfile.PAX_FORMAT)


def nlmsg(kind, flags, payload, seq=1, port=0):
    """A netlink message in this machine's byte order (as the kernel sends)."""
    body = payload + b"\0" * (-len(payload) % 4)
    return struct.pack("=IHHII", 16 + len(body), kind, flags, seq, port) + body


def nlattr(kind, value):
    """A netlink attribute, padded to 4 bytes."""
    data = struct.pack("=HH", 4 + len(value), kind) + value
    return data + b"\0" * (-len(data) % 4)


NESTED = 0x8000


def netlink_seeds():
    """What the kernel says on a netlink socket: real dumps of this machine's
    links, addresses and routes where it can be asked (Linux), and messages
    shaped like them everywhere: an error with an extended ACK, the end of a
    dump, a veth with nested link info."""
    ifinfo = struct.pack("=BxHiII", 0, 772, 1, 0x10049, 0)
    mtu = nlattr(4, struct.pack("=I", 65536))
    yield "link-lo", nlmsg(16, 2, ifinfo + nlattr(3, b"lo\0") + mtu)
    peer = nlattr(NESTED | 1, ifinfo + nlattr(3, b"eth0\0"))
    info = nlattr(NESTED | 18, nlattr(1, b"veth\0") + nlattr(NESTED | 2, peer))
    yield "link-veth", nlmsg(16, 2, ifinfo + nlattr(3, b"shy-a\0") + info)
    ifaddr = struct.pack("=BBBBI", 2, 16, 0x80, 0, 3)
    v4 = bytes([10, 88, 0, 1])
    yield "address", nlmsg(20, 2, ifaddr + nlattr(1, v4) + nlattr(2, v4))
    rtmsg = struct.pack("=BBBBBBBBI", 2, 0, 0, 0, 254, 3, 0, 1, 0)
    oif = nlattr(4, struct.pack("=I", 3))
    yield "route", nlmsg(24, 2, rtmsg + nlattr(5, v4) + oif)
    # an error as a socket with NETLINK_CAP_ACK gets it: the code, the
    # request's header alone (its length still the whole request's), then
    # the extended ACK (flags CAPPED | ACK_TLVS)
    header = struct.pack("=IHHII", 32, 16, 0x405, 7, 0)
    reason = nlattr(1, b"Attribute failed policy validation\0")
    yield "error-ext-ack", nlmsg(
        2, 0x300, struct.pack("=i", -34) + header + reason + nlattr(2, struct.pack("=I", 36)), seq=7
    )
    yield "done", nlmsg(3, 2, struct.pack("=i", 0))
    yield "done-interrupted-ext-ack", nlmsg(
        3, 0x212, struct.pack("=i", -19) + nlattr(1, b"Can not find device\0")
    )
    if not hasattr(socket, "AF_NETLINK"):
        return
    dumps = [
        ("links", 18, ifinfo),
        ("addresses", 22, struct.pack("=BBBBI", 0, 0, 0, 0, 0)),
        ("routes", 26, struct.pack("=BBBBBBBBI", 2, 0, 0, 0, 0, 0, 0, 0, 0)),
    ]
    for name, kind, family_header in dumps:
        try:
            with socket.socket(socket.AF_NETLINK, socket.SOCK_RAW, socket.NETLINK_ROUTE) as s:
                s.settimeout(2)
                s.send(nlmsg(kind, 0x301, family_header))
                for part in range(16):
                    data = s.recv(65536)
                    yield f"kernel-{name}-{part}", data
                    if done_in(data):
                        break
        except OSError:
            pass


def done_in(datagram):
    """Whether any message in `datagram` is NLMSG_DONE (the kernel often
    puts it at the end of the last datagram of data)."""
    offset = 0
    while offset + 16 <= len(datagram):
        length, kind = struct.unpack_from("=IH", datagram, offset)
        if kind == 3:
            return True
        if length < 16:
            return False
        offset += (length + 3) & ~3
    return False


# Lines of a container's log (CRI, stage 8): whole and partial records,
# both streams, offsets, an empty text, and what isn't a record.
CRI_RECORDS = {
    "full": b"2026-10-10T06:16:19.123456789Z stdout F hello",
    "partial": b"2026-10-10T06:16:19.5Z stderr P part of a line",
    "empty": b"2016-10-20T18:39:20.57606443Z stdout F",
    "offset": b"2026-10-10T08:16:19+02:00 stdout F:x more",
    "binary": b"2026-10-10T06:16:19Z stdout F \x00\xff\x1b[31m",
    "torn": b"2026-10-10T06:16:19.12",
}


def frame(kind, payload):
    return bytes([kind]) + len(payload).to_bytes(4, "big") + payload


# What an attach client sends (stage 8): a hello, typing with and without the
# detach keys, a resize; the first byte is where the fuzz target cuts.
ATTACH_FRAMES = {
    "hello-typing": b"\x07" + frame(1, b"\0\0\0\x01\x10\x11") + frame(2, b"ls\r"),
    "detach": b"\x03" + frame(2, b"ab\x10") + frame(2, b"\x11cd"),
    "resize": b"\x00" + frame(3, b"\0\x1e\0\x64"),
}


def write(directory, name, data):
    os.makedirs(directory, exist_ok=True)
    with open(os.path.join(directory, name), "wb") as out:
        out.write(data)


# `-p` specs and `ExposedPorts` keys, good and bad.
PORTS = [
    "80", "8080:80", "8080:80/udp", "127.0.0.1:8080:80", "127.0.0.1::80",
    "8000-8010:80-90", "80-90", "80/tcp", "53/udp", "[::1]:8080:80", "::1:8080:80",
    "0", "65536", "8000-8001:80", "1:2:3:4:5", "80/sctp", "a.b:1:2", "1-65535",
    "[2001:db8::1]:8080:80", "[::]::80/udp", "[2001:db8::1]", "[::1]:80:80", "[x]:1:2",
]


def dns_name(text):
    return b"".join(bytes([len(label)]) + label.encode() for label in text.split(".") if label) + b"\0"


def dns_query(qid, name, qtype, flags=0x0100):
    header = struct.pack(">HHHHHH", qid, flags, 1, 0, 0, 0)
    return header + dns_name(name) + struct.pack(">HH", qtype, 1)


def dns_seeds():
    """Queries a container sends, responses an upstream sends, and broken
    ones: a pointer loop, a forward pointer, an over-long label."""
    a_record = struct.pack(">HHHIH", 0xC00C, 1, 1, 0, 4) + bytes([10, 89, 0, 2])
    response = struct.pack(">HHHHHH", 7, 0x8580, 1, 1, 0, 0) + dns_name("db") + struct.pack(">HH", 1, 1) + a_record
    loop = struct.pack(">HHHHHH", 7, 0x0100, 1, 0, 0, 0) + b"\x01a\xc0\x0c" + struct.pack(">HH", 1, 1)
    return [
        ("a", dns_query(7, "db", 1)),
        ("aaaa", dns_query(7, "db.example.com", 28)),
        ("ptr", dns_query(7, "2.0.89.10.in-addr.arpa", 12)),
        ("ptr6", dns_query(7, ".".join("2" + "0" * 23 + "98000000df") + ".ip6.arpa", 12)),
        ("any", dns_query(7, "a1b2c3d4e5f6", 255)),
        ("status", dns_query(7, "db", 1, flags=0x1000)),
        ("response", response),
        ("loop", loop),
        ("forward", struct.pack(">HHHHHH", 7, 0, 1, 0, 0, 0) + b"\xc0\x20"),
        ("long-label", struct.pack(">HHHHHH", 7, 0, 1, 0, 0, 0) + b"\x40" + b"x" * 64 + b"\0"),
    ]


# /proc/<pid>/net tables: a listener, a connection, IPv6, UDP, and broken lines.
PROC_NET_HEADER = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n"
PROC_NET = {
    "tcp": PROC_NET_HEADER
    + "   0: 0100007F:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 31337 1 0 100 0 0 10 0\n"
    + "   1: 0200590A:0050 090200C0:D431 01 00000000:00000000 00:00000000 00000000     0        0 31338 1 0 20 4 30 10 -1\n",
    "tcp6": PROC_NET_HEADER
    + "   0: 00000000000000000000000001000000:1F90 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 31339 1\n",
    "udp": PROC_NET_HEADER
    + "  12: 00000000:0035 00000000:0000 07 00000000:00000000 00:00000000 00000000     0        0 31340 2 0 0\n",
    "udp6": PROC_NET_HEADER
    + "   0: 00000000000000000000000000000000:0035 00000000000000000000000000000000:0000 07 00000000:00000000 00:00000000 00000000     0        0 31341 2 0 0\n"
    + "   1: 00000000000000000000000001000000:9C40 00000000000000000000000001000000:0035 01 00000000:00000000 00:00000000 00000000  1000        0 31342 2 0 0\n",
    "short": PROC_NET_HEADER + "   0: 0100007F:1F90 00000000\n",
    "wide": PROC_NET_HEADER + "   0: 0100007F00:1F90 00000000:0000 0A\n",
}


# runc `process.json` files (`exec --process`): as runc's own tests and
# containerd write them, and the edges (no capabilities, a terminal, limits).
PROCESS_JSON = {
    "runc": {
        "terminal": False,
        "user": {"uid": 0, "gid": 0, "additionalGids": [10]},
        "args": ["sh", "-c", "echo hi"],
        "env": ["PATH=/usr/sbin:/usr/bin:/sbin:/bin", "TERM=xterm"],
        "cwd": "/",
        "capabilities": {
            "bounding": ["CAP_AUDIT_WRITE", "CAP_KILL", "CAP_NET_BIND_SERVICE"],
            "effective": ["CAP_AUDIT_WRITE", "CAP_KILL", "CAP_NET_BIND_SERVICE"],
            "permitted": ["CAP_AUDIT_WRITE", "CAP_KILL", "CAP_NET_BIND_SERVICE"],
            "ambient": [],
        },
        "rlimits": [{"type": "RLIMIT_NOFILE", "hard": 1024, "soft": 1024}],
        "noNewPrivileges": True,
        "oomScoreAdj": 100,
    },
    "bare": {"args": ["true"], "cwd": "/", "user": {"uid": 7, "gid": 8}},
    "terminal": {"args": ["sh"], "cwd": "/", "user": {"uid": 0, "gid": 0},
                 "terminal": True, "consoleSize": {"height": 24, "width": 80}},
}


def main():
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    root = sys.argv[1]
    for n, document in enumerate(manifest_seeds()):
        write(f"{root}/manifest", f"seed-{n}", json.dumps(document).encode())
    for n, header in enumerate(CHALLENGES):
        write(f"{root}/challenge", f"seed-{n}", header.encode())
    for n, reference in enumerate(REFERENCES):
        write(f"{root}/reference", f"seed-{n}", reference.encode())
    for name, layer in layer_seeds():
        write(f"{root}/tar", name, layer)
        write(f"{root}/unpack", name, layer)
        # cp's: a first byte for its options (no top name; one; renamed;
        # that must be a directory).
        for choice in (0, 1, 3, 7):
            write(f"{root}/cp_unpack", f"{name}-{choice}", bytes([choice]) + layer)
    for name, message in netlink_seeds():
        write(f"{root}/netlink", name, message)
    for n, spec in enumerate(PORTS):
        write(f"{root}/ports", f"seed-{n}", spec.encode())
    for name, message in dns_seeds():
        write(f"{root}/dns", name, message)
    for name, table in PROC_NET.items():
        write(f"{root}/proc_net", name, table.encode())
    for name, line in CRI_RECORDS.items():
        write(f"{root}/cri_record", name, line)
    log = b"".join(line + b"\n" for line in CRI_RECORDS.values())
    write(f"{root}/logs_dir", "records", log)
    write(f"{root}/logs_dir", "torn", log[: len(log) // 2] + log)
    for name, frames in ATTACH_FRAMES.items():
        write(f"{root}/attach_frames", name, frames)
    for name, process in PROCESS_JSON.items():
        write(f"{root}/process_json", name, json.dumps(process).encode())


if __name__ == "__main__":
    main()
