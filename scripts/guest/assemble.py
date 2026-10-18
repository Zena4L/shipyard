#!/usr/bin/env python3
"""Assembles the macOS guest from a prepared Alpine root (step 10.3a).

Out of ROOT (the minirootfs with linux-virt and e2fsprogs installed) it makes,
in OUT:

- `Image`: the raw arm64 kernel. Alpine's `vmlinuz-virt` is an EFI zboot
  image (a gzip of the kernel behind a small PE header), which
  Virtualization.framework's Linux boot loader can't boot: the payload is
  unpacked, and checked for the arm64 `ARMd` magic.
- `initramfs.cpio.gz`: the guest's whole root, as the kernel unpacks it into
  its first file system: the root without `/boot`, the static `shipyard`, an
  `/init` that hands over to it, and `/dev/console` (the kernel opens it
  before `/init` runs). Written byte for byte the same from the same inputs:
  entries sorted, every owner root, every time SOURCE_DATE_EPOCH (0).
- `manifest.json` (version 1): what it was made of, for the VM host and the
  CLI to check (`build`: the repository's revision and the binary's sha256).
"""

import argparse
import gzip
import hashlib
import io
import json
import os
import shutil
import stat
import struct
import sys

MANIFEST_VERSION = 1
EPOCH = int(os.environ.get("SOURCE_DATE_EPOCH", "0"))
INIT = b"#!/bin/sh\n# The guest's init is shipyard's (step 10.3a).\nexec /usr/bin/shipyard __guest-init\n"


def raw_kernel(vmlinuz: bytes) -> bytes:
    """The raw kernel inside an EFI zboot image (or the image itself)."""
    if vmlinuz[:2] == b"MZ" and vmlinuz[4:8] == b"zimg":
        offset, size = struct.unpack_from("<II", vmlinuz, 8)
        kind = vmlinuz[0x18:0x20].rstrip(b"\0")
        if kind != b"gzip":
            sys.exit(f"the kernel is compressed with {kind!r}: only gzip is unpacked")
        kernel = gzip.decompress(vmlinuz[offset : offset + size])
    else:
        kernel = vmlinuz
    if kernel[0x38:0x3C] != b"ARMd":
        sys.exit("not an arm64 kernel image (no ARMd magic at 0x38)")
    return kernel


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


class Cpio:
    """A newc ("070701") archive, as the kernel's initramfs unpacker reads."""

    def __init__(self) -> None:
        self.out = io.BytesIO()
        self.ino = 1

    def _entry(self, name: str, mode: int, data: bytes = b"", rdev=(0, 0)) -> None:
        encoded = name.encode() + b"\0"
        fields = [
            self.ino, mode, 0, 0, 1, EPOCH, len(data), 0, 0, rdev[0], rdev[1], len(encoded), 0,
        ]
        self.ino += 1
        self.out.write(b"070701" + b"".join(b"%08X" % f for f in fields) + encoded)
        self.out.write(b"\0" * (-(110 + len(encoded)) % 4))
        self.out.write(data)
        self.out.write(b"\0" * (-len(data) % 4))

    def add_tree(self, root: str) -> None:
        paths = []
        for directory, dirs, files in os.walk(root):
            dirs.sort()
            for entry in sorted(dirs + files):
                paths.append(os.path.join(directory, entry))
        for path in sorted(paths):
            name = os.path.relpath(path, root)
            info = os.lstat(path)
            mode = info.st_mode
            if stat.S_ISLNK(mode):
                self._entry(name, mode, os.readlink(path).encode())
            elif stat.S_ISDIR(mode):
                self._entry(name, mode)
            elif stat.S_ISREG(mode):
                with open(path, "rb") as file:
                    self._entry(name, mode, file.read())
            else:
                sys.exit(f"{name}: not a file, a directory or a link")

    def add_device(self, name: str, major: int, minor: int, mode: int = 0o600) -> None:
        self._entry(name, stat.S_IFCHR | mode, rdev=(major, minor))

    def finish(self) -> bytes:
        self._entry("TRAILER!!!", 0)
        return self.out.getvalue()


def main() -> None:
    parser = argparse.ArgumentParser()
    for flag in ("--root", "--binary", "--installed", "--out", "--revision", "--alpine",
                 "--minirootfs-sha256"):
        parser.add_argument(flag, required=True)
    args = parser.parse_args()
    root = args.root

    with open(os.path.join(root, "boot", "vmlinuz-virt"), "rb") as file:
        kernel = raw_kernel(file.read())
    (kernel_version,) = os.listdir(os.path.join(root, "lib", "modules"))
    shutil.rmtree(os.path.join(root, "boot"))

    with open(args.binary, "rb") as file:
        binary = file.read()
    target = os.path.join(root, "usr", "bin", "shipyard")
    with open(target, "wb") as file:
        file.write(binary)
    os.chmod(target, 0o755)
    with open(os.path.join(root, "init"), "wb") as file:
        file.write(INIT)
    os.chmod(os.path.join(root, "init"), 0o755)

    archive = Cpio()
    archive.add_tree(root)
    if not os.path.exists(os.path.join(root, "dev", "console")):
        archive.add_device("dev/console", 5, 1)
    initramfs = gzip.compress(archive.finish(), compresslevel=9, mtime=0)

    # `apk info -v`: one installed package a line, as NAME-VERSION.
    with open(args.installed) as file:
        packages = {line.strip() for line in file if line.strip()}
    manifest = {
        "version": MANIFEST_VERSION,
        "alpine": args.alpine,
        "minirootfs_sha256": args.minirootfs_sha256,
        "packages": sorted(packages),
        "kernel": {"version": kernel_version, "sha256": sha256(kernel)},
        "initramfs": {"sha256": sha256(initramfs)},
        "shipyard": {"revision": args.revision, "sha256": sha256(binary)},
    }
    # Each written beside, then renamed in, the manifest last: a build cut
    # short never leaves a manifest naming files that aren't there (and the
    # VM host checks the files' sha256s against it anyway).
    text = (json.dumps(manifest, indent=2) + "\n").encode()
    for name, data in (("Image", kernel), ("initramfs.cpio.gz", initramfs), ("manifest.json", text)):
        path = os.path.join(args.out, name)
        with open(path + ".new", "wb") as file:
            file.write(data)
            file.flush()
            os.fsync(file.fileno())
        os.replace(path + ".new", path)


if __name__ == "__main__":
    main()
