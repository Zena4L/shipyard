# Compose

`shipyard compose` runs Docker's own [`docker compose`](https://docs.docker.com/compose/)
against Shipyard: compose talks to Shipyard's [Docker API](api.md) as it would to
Docker's daemon. Shipyard has no compose of its own (D27).

```sh
shipyard compose up -d
shipyard compose ps
shipyard compose logs -f web
shipyard compose exec web sh
shipyard compose down -v
```

Everything after `compose` is compose's: its commands, flags and files
(`compose.yaml` in the current directory, `-f`, `-p`, `--profile`…), its output and
its exit code.

## What it needs

Docker's CLI (`docker`) and its compose plugin, from Docker's releases or your
distribution (`docker-ce-cli` and `docker-compose-plugin`, or the static binaries).
No Docker daemon: Shipyard is the one compose talks to.

## How it reaches Shipyard

`shipyard compose` sets `DOCKER_HOST` to Shipyard's API socket
(`/run/shipyard/api.sock` for root, `$XDG_RUNTIME_DIR/shipyard/api.sock` for
anyone else), then becomes `docker compose` (its signals, terminal and exit code are
compose's). If nothing serves that socket yet (systemd's `shipyard.socket` isn't
installed: see [the API](api.md)), it starts `shipyard system service` itself; that
service ends once nothing has asked it anything for 60 seconds, so nothing is left
running. With the socket units installed, systemd starts it instead.

`docker compose` can also be run directly, with `DOCKER_HOST` set as above.

## What works

Images pulled as needed; a project's network and named volumes made and removed
(`down -v`); `depends_on` with `service_healthy` (Shipyard's health checks);
published ports; labels; `ps`, `logs`, `exec`, `run` (`--rm`), `top`, `stop`,
`start`, `restart`; an attached `up` (`--abort-on-container-exit`,
`--exit-code-from`), which follows its containers' events by their labels;
profiles.

Volumes as Docker makes them: anonymous ones (`volumes: [/data]`, an image's
own `VOLUME`s), removed by `down -v`; a new volume filled with the image's files
at its place (`volume: {nocopy: true}` asks for none); a short-syntax bind's
missing host directory (`./data:/data`) made.

A network a stopped container still names is removed by `down`, as Docker's is (see
[`network rm`](networking.md#shipyard-network-rm-network)).

## Not yet

- **`build`:** images are built in a later stage; use `image:`.
- **Rootless, a project's network:** a normal user's Shipyard has no shared networks,
  so a rootless project's services need `network_mode: none` (or `host`, `pasta`, or
  `service:NAME`, another service's), and can't reach each other by name. (`ipc:
  "service:NAME"` shares another service's IPC namespace and `/dev/shm`, root and
  rootless.)
- **On a Mac:** `shipyard compose` runs against the Mac's `docker.sock` (the VM's API),
  started as `vm start` starts it: a project's binds must be under your home, as any
  `docker run -v` there ([macOS](macos.md)).
