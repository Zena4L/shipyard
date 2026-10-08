#!/usr/bin/env python3
"""Writes the fuzz targets' starting corpora into DIR/<target>/.

Each target starts from real-shaped inputs, so the fuzzer spends its time on
the edges of valid documents and archives, not on finding a first `{` or a
tar header: registry documents as Docker Hub serves them (`null` lists
included), `WWW-Authenticate` headers from real registries, references, and
small layers, the hostile ones among them (escapes through symlinks, `..`,
hard links and whiteouts).

Usage: fuzz-seeds.py DIR
"""

import io
import json
import os
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


def write(directory, name, data):
    os.makedirs(directory, exist_ok=True)
    with open(os.path.join(directory, name), "wb") as out:
        out.write(data)


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


if __name__ == "__main__":
    main()
