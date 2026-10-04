# Shipyard documentation

Shipyard is a lightweight, daemonless container engine for macOS and Linux.

> **Status: early development.** Shipyard doesn't isolate commands yet. Today
> `shipyard run` starts a command the way a container runtime does (clean
> environment, chosen user, working directory and resource limits) and reports
> exactly how it ended. These docs describe only what works in the current
> version, and they are updated with every release.

## Pages

| Page | What it covers |
|---|---|
| [Running commands](run.md) | `shipyard run`: environment, users, working directory, resource limits, stopping |
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

The binary is `target/release/shipyard`. Copy it somewhere on your `PATH`, for example:

```sh
sudo install -m 0755 target/release/shipyard /usr/local/bin/shipyard
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

Run a command:

```sh
shipyard run -e GREETING=hello sh -c 'echo "$GREETING, $(id -un)"; exit 3'
echo $?
```

```text
hello, yourname
3
```

See [Running commands](run.md) for everything `run` accepts.

## Getting more output

Every command accepts these flags:

| Flag | Effect |
|---|---|
| `-D`, `--debug` | Show debug logs |
| `--log-level LEVEL` | `error`, `warn` (default), `info`, `debug` or `trace` |

The `RUST_LOG` environment variable also sets the log level, but a flag wins over
it. Logs go to stderr, so they never mix with a command's output.
