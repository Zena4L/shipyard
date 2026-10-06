# Shipyard documentation

Shipyard is a lightweight, daemonless container engine for macOS and Linux.

> **Status: early development.** On Linux, `shipyard run` starts a command in
> its own namespaces: its own processes (under a small init), mounts, hostname
> and IPC, and optionally a root filesystem of its own (`--rootfs`), or a whole
> container from an [OCI bundle](bundles.md) (`--bundle`); `shipyard-runtime` runs
> bundles step by step, as an [OCI runtime](runtime.md). It doesn't have its own
> network yet, and it isn't a security boundary yet. It starts the command the way a
> container runtime does (clean environment, chosen user, working directory and
> resource limits), supervises it (passing signals on, stopping it gracefully, cleaning up what it
> leaves behind), and reports exactly how it ended. On macOS, commands aren't
> isolated yet. These docs describe only what works in the current version, and
> they are updated with every release.

## Pages

| Page | What it covers |
|---|---|
| [Running commands](run.md) | `shipyard run`: isolation, environment, users, working directory, resource limits, stopping |
| [OCI bundles](bundles.md) | `shipyard run --bundle`: a container described by a `config.json`, what's supported, refused and ignored |
| [The OCI runtime](runtime.md) | `shipyard-runtime`: `create`, `start`, `state`, `kill` and `delete`, one step at a time, as `runc` |
| [Runtime reference](runtime-cli.md) | Every `shipyard-runtime` command and flag, generated from the binary itself |
| [Exit codes](exit-codes.md) | What each exit code means, for scripts that check them |
| [Command reference](cli.md) | Every command and flag, generated from the CLI itself |

## Install from source

You need [Rust](https://rustup.rs). The repository pins the exact toolchain
version, and `rustup` installs it automatically on the first build.

```sh
git clone https://github.com/Zena4L/shipyard.git
cd shipyard
cargo build --release
```

The binaries are `target/release/shipyard` and `target/release/shipyard-runtime` (the
[OCI runtime](runtime.md)). Copy them somewhere on your `PATH`, for example:

```sh
sudo install -m 0755 target/release/shipyard /usr/local/bin/shipyard
sudo install -m 0755 target/release/shipyard-runtime /usr/local/bin/shipyard-runtime
```

Install it with mode `0755` as shown. Shipyard refuses to run if it is installed
setuid or setgid.

## Quick start

Check how your host will run containers:

```sh
shipyard doctor
```

```text
host:    macos / aarch64
backend: linux-vm (not implemented yet)
```

On Linux, containers run directly on the host kernel. On macOS they will run
inside a lightweight Linux VM that Shipyard manages.

Run a command. On Linux, isolation needs root (running without root comes
later); add `--no-isolation` to run without namespaces instead:

```sh
sudo shipyard run -e GREETING=hello sh -c 'echo "$GREETING from PID $$"; exit 3'
echo $?
```

```text
hello from PID 2
3
```

PID 2: the command sees only its own processes, under Shipyard's small init
(PID 1). On macOS, run it without `sudo`; the PID is then an ordinary one.

See [Running commands](run.md) for everything `run` accepts.

## Getting more output

Every command accepts these flags:

| Flag | Effect |
|---|---|
| `-D`, `--debug` | Show debug logs |
| `--log-level LEVEL` | `error`, `warn` (default), `info`, `debug` or `trace` |

The `RUST_LOG` environment variable also sets the log level, but a flag wins over
it. Logs go to stderr, so they never mix with a command's output.
