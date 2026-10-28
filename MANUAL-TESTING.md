# Shipyard manual testing checklist

Everything Shipyard does today (stages 1 to 14), to try by hand as a user would. Each
item says what to run and what you should see. The expected outcomes come from the user
docs (`docs/user/`), the CHANGELOG and the CLI's own help (`docs/user/cli.md`).

## How to use this

- Tick an item when you've tried it: `- [ ]` becomes `- [x]`.
- At the end of the line, write ✅ (works as expected) or ❌ (doesn't), and a short note:
  what you saw, the exit code, the error's words. Example:
  `- [x] … Expect: … ✅` or `- [x] … Expect: … ❌ printed "…" and exited 1`.
- Check exit codes with `echo $?` right after the command.
- Tags say where an item runs:
  - **[Mac]**: on the Mac only (the VM, the app on macOS).
  - **[Linux]**: on a Linux host only.
  - **[root]** / **[rootless]**: as root (`sudo shipyard …`) or as a normal user, on Linux.
  - No tag: anywhere. On the Mac, every command but `vm`, `doctor`, `login`, `logout`,
    `--help` and `--version` runs in Shipyard's VM, as root there, so bridge networks
    and names between containers work on the Mac too.
- Clean up at the end of each section (the commands are given), so the next one
  starts from a known state.
- Write down at the top of your notes: `shipyard version`, `shipyard -v`, your macOS
  version, and on Linux the distribution and `uname -r`.

## Setup

### On the Mac

- [x] Build and sign `shipyard` (once after each build):
  ```sh
  cd ~/Desktop/shipyard
  cargo build
  scripts/macos/make-signing-identity.sh      # once: a local signing identity, so macOS asks once
  scripts/macos/sign.sh target/debug/shipyard
  export PATH="$PWD/target/debug:$PATH"
  ```
  Expect: `which shipyard` is `…/target/debug/shipyard`; `shipyard -v` prints its version.
- [x] Build the guest (the VM's Linux), if `target/guest` isn't there or is older than this
  tree: `scripts/guest/build.sh`. Expect: `target/guest` exists; a `shipyard` built in the
  tree finds it by itself.
- [x] Make a scratch directory **in your home, outside Desktop, Documents and Downloads**
  (those need macOS's permission): `mkdir -p ~/sy-test && cd ~/sy-test`.
- [x] Docker's client for the compatibility parts: Docker's CLI and its compose plugin
  (Docker's static binaries or a package manager; no Docker daemon is needed).
  `docker --version` and `docker compose version` both print a version.
- [x] Docker's socket on the Mac (from `docs/user/macos.md`):
  ```sh
  export DOCKER_HOST="unix://$HOME/Library/Application Support/Shipyard/vm/docker.sock"
  ```
  Or a context: `docker context create shipyard --docker "host=unix://$HOME/Library/Application Support/Shipyard/vm/docker.sock"`, then `docker --context shipyard …`.
- [x] The desktop app (needs Node.js 26):
  ```sh
  cd ~/Desktop/shipyard/desktop/ui
  npm ci
  npm run tauri -- build --bundles app
  cd ../..
  scripts/macos/sign.sh --bundle
  open desktop/target/release/bundle/macos/Shipyard.app
  ```
  If the app says it can't find `shipyard`: Settings (⌘,) → **Choose shipyard…** →
  `~/Desktop/shipyard/target/debug/shipyard`. (For development, `SHIPYARD_BIN="$PWD/../../target/debug/shipyard" npm run tauri dev` from `desktop/ui` runs it live.)

### On Linux (a real host or a Lima VM: Ubuntu 24.04 with AppArmor, Fedora with SELinux)

- [ ] Install the binaries (built with `cargo build --release`, or a static musl build):
  ```sh
  sudo install -m 0755 target/release/shipyard /usr/local/bin/shipyard
  sudo install -m 0755 target/release/shipyard-runtime /usr/local/bin/shipyard-runtime
  ```
  Expect: mode 0755. (Installed setuid or setgid, Shipyard refuses to run.)
- [ ] Rootless needs: `sudo apt-get install uidmap passt` (Debian, Ubuntu) or
  `sudo dnf install shadow-utils passt` (Fedora); a range for you in `/etc/subuid` and
  `/etc/subgid`.
- [ ] Ubuntu 23.10 and later: the AppArmor profile:
  ```sh
  sudo install -m 0644 contrib/apparmor/shipyard /etc/apparmor.d/shipyard
  sudo apparmor_parser -r /etc/apparmor.d/shipyard
  ```
- [ ] The API's socket units (for the API sections), root's and yours:
  ```sh
  sudo install -m 0644 contrib/systemd/shipyard.socket contrib/systemd/shipyard.service /etc/systemd/system/
  sudo systemctl daemon-reload && sudo systemctl enable --now shipyard.socket
  mkdir -p ~/.config/systemd/user
  install -m 0644 contrib/systemd/user/shipyard.socket contrib/systemd/user/shipyard.service ~/.config/systemd/user/
  systemctl --user daemon-reload && systemctl --user enable --now shipyard.socket
  ```
- [ ] `DOCKER_HOST` on Linux: rootless `export DOCKER_HOST=unix://$XDG_RUNTIME_DIR/shipyard/api.sock`;
  root's `export DOCKER_HOST=unix:///run/shipyard/api.sock` and `sudo -E docker …`.
- [ ] Docker's CLI and compose plugin (`docker-ce-cli`, `docker-compose-plugin`), no daemon.
- [ ] A login session with a delegated cgroup (a desktop terminal, or `systemd-run --user --scope …` from SSH) for rootless limits, `pause`, `stats`, `build`.

---

## 1. CLI basics: version, info, doctor, help

- [x] `shipyard -v` (also `-V`, `--version`). Expect: the version, exit 0. On a Mac it's the Mac's own `shipyard`.
- [x] `shipyard --help`. Expect: the subcommand list (run, create, start, … vm, builder, doctor, version, info), exit 0.
- [x] `shipyard` with no command. Expect: its help, exit **125**.
- [x] `shipyard run --no-such-flag alpine`. Expect: a usage error, exit 125.
- [x] `shipyard version`. Expect: a `Client` part (version, API version 1.43, revision, OS/arch) and a `Server` part (API version 1.43, oldest API 1.24). On a Mac both describe the VM's `shipyard`.
- [x] `shipyard version --format '{{.Server.APIVersion}}'`. Expect: `1.43`.
- [x] `shipyard version --format json`. Expect: one JSON object.
- [x] `shipyard info`. Expect: containers (running, paused, stopped), images, storage and logging drivers, cgroup driver and version, security options, kernel, distribution, CPUs, memory, host name.
- [x] `shipyard info --format '{{.NCPU}}'`. Expect: a number.
- [x] `shipyard info --format '{{.Nope}}'`. Expect: the template fails, exit 1.
- [x] `shipyard doctor` **[Linux]** (as you and as root). Expect: `host:`, `backend:`, `user:` lines, then one `ok`/`warn`/`fail` line each (user namespaces, ID mapping, cgroups, images, network, runtime state, linger, restart at boot; as root a `firewall` line), then `containers run here`. Exit 0 unless something failed (then 1).
- [x] `shipyard doctor` **[Mac]**. Expect: `host: macos / aarch64`, `backend: linux-vm (Virtualization.framework)`, then `macos`, `entitlement` (says by whom it's signed), `guest`, `vm`, `ports`, `shares`, `files`, `disk`, `rosetta` lines. With the VM running, the VM's own Linux checks after (`guest: Linux 6.18…`, cgroups, images, network, runtime state). With the VM stopped: says it isn't started to be checked.
- [x] `shipyard -D ps` and `shipyard --log-level debug ps`. Expect: debug logs on stderr, the table on stdout.
- [x] `RUST_LOG=debug shipyard --log-level error ps`. Expect: no debug lines (the flag wins).
- [ ] `shipyard ps | head -0; echo $?`. Expect: 0, quiet (a reader that goes isn't a failure). -> I got this outpu `clementbogyah@Clements-MacBook-Pro sy-test % shipyard ps | head -0; echo $?
head: illegal line count -- 0
shipyard: warning: the running VM booted the guest in /Users/clementbogyah/Library/Application Support/Shipyard/milestone-13/guest, not this shipyard's (/Users/clementbogyah/Desktop/shipyard/target/guest): `shipyard vm stop`, and it starts again with this one
  1
``
- [x] Every `container …` and `image …` alias works: `shipyard container ls`, `shipyard container list`, `shipyard image ls`, `shipyard image rm …`. Expect: same as `ps`, `images`, `rmi`.

## 2. Running a container in the foreground

Use `alpine:3.20`, `nginx:alpine` and `busybox` throughout.

### Basics and exit codes

- [x] `shipyard run alpine echo hi`. Expect: first time `Pulling docker.io/library/alpine:latest`, a downloaded line, then `hi`; exit 0.
- [x] `shipyard ps -a`. Expect: the container `Exited (0) …` with a made-up `adjective_noun` name.
- [x] `shipyard logs <that name>`. Expect: `hi`.
- [x] `shipyard start -a <that name>`. Expect: `hi` again, exit 0.
- [x] `shipyard run --rm alpine echo hi; shipyard ps -a`. Expect: `hi`; the container is gone once `run` returns.
- [x] `shipyard run alpine sh -c 'echo "hello from PID $$"; exit 3'; echo $?`. Expect: `hello from PID 2`, then `3`.
- [x] `shipyard run alpine sh -c 'kill -TERM $$'; echo $?`. Expect: 143.
- [x] `shipyard run alpine nosuchprogram; echo $?`. Expect: 127, a message starting `shipyard:` or `error:`.
- [x] `shipyard run alpine /etc; echo $?`. Expect: 126 (a directory can't be run).
- [x] `shipyard run nosuchimage-xyz:1; echo $?`. Expect: 125, the pull's error.
- [x] `shipyard run; echo $?`. Expect: 125 (a run needs an image or `--no-image`).
- [x] `shipyard run alpine:3.20` (no command). Expect: the image's own `CMD` (`/bin/sh`) runs and ends at once (no input), exit 0.
- [x] `shipyard run alpine:3.20 cat /etc/alpine-release`. Expect: `3.20.x`.
  - [x] `shipyard run -e A=1 alpine ls -l --env X; echo $?`. Expect: `ls` itself complains about `--env` (options after the image are the command's).
    - [x] `shipyard run alpine --rm`. Expect: tries to run `--rm` in the container (not taken as Shipyard's flag).
    - [x] `shipyard run --no-image -- --program-starting-with-dash` **[Linux]**. Expect: 127 (not found), not a usage error.

### Input

- [ ] `printf 'a\nb\n' | shipyard run -i alpine cat`. Expect: `a` and `b`.
- [ ] `printf 'a\n' | shipyard run alpine cat`. Expect: nothing (without `-i` the container reads `/dev/null`).
- [ ] `shipyard run -i alpine sh` then type `echo typed` Enter, then Ctrl-D. Expect: `typed`, then the run ends.
- [ ] `echo x | shipyard run -it alpine sh; echo $?`. Expect: "the input device is not a TTY", 125.
- [ ] `shipyard run -a stdout alpine sh -c 'echo out; echo err >&2'`. Expect: only `out`.
- [ ] `shipyard run -a stdin alpine cat; echo $?`. Expect: refused (`-a stdin` needs `-i`).
- [ ] `shipyard run -d -a stdout alpine true; echo $?`. Expect: refused (`-a` not with `-d`).
- [ ] `shipyard run alpine yes | head -1; echo ${PIPESTATUS[0]}` (bash). Expect: `y`; the container's writer gets SIGPIPE (`141` if that ends it).
- [ ] `shipyard run alpine seq 1 100000 | (sleep 5; wc -l)`. Expect: `100000` (a slow reader loses nothing).
- [ ] `seq 1 200000 > big.txt; shipyard run -i alpine wc -l < big.txt`. Expect: `200000`.

### A terminal of the container's own (`-it`)

- [ ] `shipyard run -it alpine sh`, then `tty`. Expect: `/dev/pts/0`.
- [ ] In it, `top`, then `q`. Expect: full-screen top, your terminal fine after.
- [ ] In it, `vi /tmp/x`, type text, `:wq`. Expect: vi works full-screen.
- [ ] In it, resize the terminal window, then `stty size`. Expect: the new size.
- [ ] In it, `sleep 100` then Ctrl-C. Expect: the sleep ends, the shell stays (Ctrl-C is the container's key).
- [ ] In it, `sleep 100` then Ctrl-Z, then `jobs`, `fg`, Ctrl-C. Expect: the job stops and resumes inside the container; Shipyard isn't stopped.
- [ ] In it, `sleep 100` then Ctrl-\\. Expect: the sleep quits (core/quit), shell stays.
- [ ] In it, Ctrl-D. Expect: the shell exits, run exits 0, your terminal is put back (not raw).
- [ ] `shipyard run -it --name dt alpine sh`, then Ctrl-P Ctrl-Q. Expect: detached, `run` exits 0; `shipyard ps` shows `dt` running.
- [ ] `shipyard attach dt`, then `echo back`, then Ctrl-P Ctrl-Q. Expect: back in the shell (you may need Enter to see a prompt); detaching again exits 0.
- [ ] `shipyard run -it --detach-keys ctrl-a,d --name dk alpine sh`, then Ctrl-A D. Expect: detached. Then `shipyard rm -f dt dk`.
- [ ] `shipyard run -it --detach-keys= alpine sh`, Ctrl-P Ctrl-Q. Expect: nothing detaches (the keys go to the shell); `exit` to leave.
- [ ] `shipyard run -t alpine ls --color=always /bin | cat`. Expect: works without `-i`, output to a pipe.
- [ ] `shipyard run -it alpine sh &` (in bash), then `fg`. Expect: it waits for `fg` before taking your terminal, then works.
- [ ] Close the terminal window while `shipyard run -it --name hup alpine sh` runs; then from another terminal `shipyard ps -a`. Expect: the run ended (129) or the container ended of SIGHUP.
- [ ] `shipyard run -it alpine sh -c 'su -c true nobody'` (or any password prompt). Expect: the prompt appears on the container's terminal.
- [ ] Escape sequences: `shipyard run alpine printf '\033]0;TITLE\007'`. Expect: your terminal's title changes (passed on as it is, as Docker does: noted, not a bug).

### Signals (no terminal)

- [ ] `shipyard run alpine sleep 100`, then Ctrl-C. Expect: exit 130, the container ends.
- [ ] `shipyard run --sig-proxy=false --name sp alpine sleep 100`, Ctrl-C. Expect: `run` ends; `shipyard ps` shows `sp` still running. Then `shipyard rm -f sp`.
- [ ] `timeout 3 shipyard run --stop-timeout 2 alpine sleep 100; echo $?`. Expect: SIGTERM is the container's stop (its stop signal, then SIGKILL after 2 s); exit 143 or 137.
- [ ] `shipyard run --name k9 alpine sleep 300 &` then `kill -9 %1`; `shipyard ps`. Expect: `k9` runs on (as with `-d`). Then `shipyard rm -f k9`.
- [ ] `shipyard run --stop-signal SIGSTOP alpine true; echo $?`. Expect: refused, 125.
- [ ] `shipyard run --stop-signal RTMIN+3 alpine true` **[Linux]**. Expect: accepted (signal 37).
- [ ] In bash: `for i in 1 2 3; do shipyard run alpine sleep 100; done`, Ctrl-C once. Expect: the loop stops too.

### Environment, user, working directory, entrypoint

- [ ] `shipyard run alpine env`. Expect: only `PATH`, `HOME=/root`, `HOSTNAME` and the image's variables (none of your shell's).
- [ ] `FOO=bar shipyard run -e FOO -e A==b -e EMPTY= alpine env`. Expect: `FOO=bar`, `A==b`, `EMPTY=`.
- [ ] `shipyard run -e 'BAD NAME=1' alpine true; echo $?`. Expect: refused, 125.
- [ ] `printf 'DB_HOST=db\nDB_PASSWORD=s3cret\n# not read\nHOME\n' > app.env; shipyard run --env-file app.env -e DB_HOST=other alpine env`. Expect: `DB_HOST=other`, `DB_PASSWORD=s3cret`.
- [ ] `shipyard -D run -e TOKEN=supersecret alpine true 2>&1 | grep supersecret`. Expect: nothing (values are never shown).
- [ ] `shipyard run -u nobody alpine id`. Expect: `uid=65534(nobody)`.
- [ ] `shipyard run -u 1000:1000 alpine id`. Expect: `uid=1000 gid=1000`.
- [ ] `shipyard run -u nosuchuser alpine id; echo $?`. Expect: says which name and which file, 125.
- [ ] `shipyard run -u 4294967295 alpine id; echo $?`. Expect: refused, 125.
- [ ] `shipyard run --group-add 10 alpine id`. Expect: groups include 10.
- [ ] `shipyard run -w /srv alpine pwd`. Expect: `/srv` (made if missing).
- [ ] `shipyard run -w relative alpine pwd; echo $?`. Expect: refused, 125.
- [ ] `shipyard run -w / node:20 pwd`. Expect: `/`.
- [ ] `shipyard run --entrypoint /bin/echo alpine:3.20 hi`. Expect: `hi`.
- [ ] `shipyard run nginx:alpine nginx -v`. Expect: nginx's version (the entrypoint, your arguments).
- [ ] `shipyard run --entrypoint "" alpine echo alone`. Expect: `alone`.
- [ ] `shipyard run --pull never nosuch-image:9; echo $?`. Expect: error, nothing pulled.
- [ ] `shipyard run --pull always alpine:3.20 true`. Expect: asks the registry again (pull lines).
- [ ] `shipyard run -q alpine:3.19 true`. Expect: no pull progress.

### Isolation, hostname, sysctls, capabilities, seccomp, ulimits

- [ ] `shipyard run alpine ps`. Expect: only `shipyard-init` (PID 1) and `ps` (PID 2 or so).
- [ ] `shipyard run alpine hostname`. Expect: 12 hex digits (the container's short ID).
- [ ] `shipyard run -h web-1 --domainname corp.internal alpine sh -c 'hostname; domainname'`. Expect: `web-1`, `corp.internal`.
- [ ] `shipyard run -h -bad- alpine true; echo $?`. Expect: refused, 125.
- [ ] `shipyard run --sysctl kernel.shmmax=1073741824 alpine cat /proc/sys/kernel/shmmax`. Expect: `1073741824`.
- [ ] `shipyard run --sysctl net.ipv4.ip_forward=1 alpine cat /proc/sys/net/ipv4/ip_forward`. Expect: `1`.
- [ ] `shipyard run --sysctl vm.swappiness=10 alpine true; echo $?`. Expect: refused (would change the whole machine), 125.
- [ ] `shipyard run --network host --sysctl net.ipv4.ip_forward=1 alpine true; echo $?`. Expect: refused.
- [ ] `shipyard run alpine cat /proc/sys/net/ipv4/ip_unprivileged_port_start`. Expect: `0`.
- [ ] `shipyard run alpine grep Cap /proc/self/status`. Expect: a reduced set (Docker's 14).
- [ ] `shipyard run alpine ip link add dummy0 type dummy; echo $?`. Expect: "Operation not permitted" (no NET_ADMIN).
- [ ] `shipyard run --cap-add NET_ADMIN alpine ip link add dummy0 type dummy; echo $?`. Expect: works, 0.
- [ ] `shipyard run --cap-add NET_ADMIN --cap-drop NET_ADMIN alpine true; echo $?`. Expect: refused (same in both), 125.
- [ ] `shipyard run --cap-drop ALL alpine chown 1 /tmp; echo $?`. Expect: Operation not permitted.
- [ ] `shipyard run -u 1000 --cap-add NET_BIND_SERVICE alpine nc -l -p 80 & sleep 1; kill %1`. Expect: it binds.
- [ ] `shipyard run alpine unshare -U true; echo $?`. Expect: Operation not permitted (seccomp).
- [ ] Seccomp profile: write `nomkdir.json` with the docs' example (`mkdir`, `mkdirat` → errno 20), then `shipyard run --security-opt seccomp=nomkdir.json alpine mkdir /x; echo $?`. Expect: "Not a directory".
- [ ] `shipyard run --security-opt seccomp=/nonexistent alpine true; echo $?`. Expect: an error naming the file, 125.
- [ ] `shipyard run --security-opt seccomp=unconfined alpine unshare -U true`. Expect: works.
- [ ] `shipyard run alpine sh -c 'grep NoNewPrivs /proc/self/status'`. Expect: `NoNewPrivs: 1`; with `--security-opt no-new-privileges=false`, `0`.
- [ ] `shipyard run --ulimit nofile=1024:2048 alpine sh -c 'ulimit -n; ulimit -Hn'`. Expect: `1024`, `2048`.
- [ ] `shipyard run --ulimit nofile=unlimited alpine true; echo $?` **[Linux / Mac VM]**. Expect: refused, 125.
- [ ] `shipyard run --ulimit core=0 alpine sh -c 'ulimit -c'`. Expect: `0`.
- [ ] `shipyard run --oom-score-adj 500 alpine cat /proc/self/oom_score_adj`. Expect: `500`.
- [ ] `shipyard run --oom-score-adj -500 alpine true` **[rootless]**. Expect: refused (can only go up).
- [ ] `shipyard run alpine cat /proc/kcore | wc -c`. Expect: 0 (hidden).
- [ ] `shipyard run alpine sh -c 'echo 1 > /proc/sys/kernel/printk'; echo $?`. Expect: read-only file system.
- [ ] `shipyard run --read-only alpine touch /x; echo $?`. Expect: read-only file system; `touch /dev/shm/x` still works.
- [ ] `shipyard run alpine ls /sys/class/net`. Expect: only the container's own interfaces.
- [ ] `shipyard run alpine cat /proc/self/cgroup`. Expect: `0::/`.
- [ ] `shipyard run --privileged alpine true; echo $?`. Expect: refused, saying to use `--cap-add`/`--device`.
- [ ] `shipyard run --init alpine true; echo $?`. Expect: refused, saying to run without it.
- [ ] `shipyard run --name '<b>x</b>' alpine true; echo $?`. Expect: refused (names are letters, digits, `_`, `-`), 125. **(hostile)**

### Linux-only run modes

- [ ] **[Linux]** `shipyard run --no-image sh -c 'echo hi; ps'`. Expect: `hi`, only its own processes.
- [ ] **[Linux][root]** `sudo shipyard run --no-image true`. Expect: a warning "the container is root on the host's own files".
- [ ] **[Linux]** `shipyard run --no-isolation true; echo $?`. Expect: 0, runs on the host.
- [ ] **[Linux]** `mkdir rootfs && shipyard export <some container> | tar -x -C rootfs; shipyard run --rootfs ./rootfs sh -c 'ls /'`. Expect: the rootfs's files; `/proc`, `/sys`, `/dev` added.
- [ ] **[Linux]** `shipyard run --rootfs / sh; echo $?`. Expect: refused (`--no-image` for the host's files), 125.
- [ ] **[Linux][root]** `sudo shipyard run --uidmap 0:100000:65536 --no-image sh -c 'id; cat /proc/self/uid_map'`. Expect: `uid=0(root)`, the map `0 100000 65536`.
- [ ] **[Linux][rootless]** `shipyard run --no-image sh -c 'id; cat /proc/self/uid_map'`. Expect: root inside; map `0 <your uid> 1` and `1 100000 65536`.
- [ ] **[Linux]** `shipyard run --device /dev/fuse --rootfs ./rootfs ls -l /dev/fuse`. Expect: the device inside.
- [ ] **[Linux]** `shipyard run --no-image cat /dev/sda 2>&1 | head -1`. Expect: Operation not permitted (device control).

## 3. Containers in the background

### Making and starting

- [ ] `shipyard run -d --name web -p 8080:80 nginx:alpine`. Expect: prints the full ID (64 hex), exit 0.
- [ ] `curl -s localhost:8080 | grep -o '<title>.*</title>'`. Expect: `<title>Welcome to nginx!</title>`.
- [ ] `shipyard create --name db -e POSTGRES_PASSWORD=x postgres:16`. Expect: prints an ID; `ps -a` shows `Created`.
- [ ] `shipyard start db`. Expect: prints `db`; `ps` shows it `Up …`.
- [ ] `shipyard start db` again. Expect: left as it is, prints `db`, exit 0.
- [ ] `shipyard run -d --name web nginx:alpine; echo $?`. Expect: refused (name taken), exit 125.
- [ ] `shipyard run -d --cidfile cid.txt alpine sleep 60; cat cid.txt`. Expect: the ID in the file. Run it again with the same file: refused before anything is pulled or made.
- [ ] `shipyard run -d -l tier=web --label-file labels.txt alpine sleep 60` (with `tier=file` in the file). Expect: `inspect` shows `tier=web` (`-l` wins).
- [ ] `shipyard run -d --expose 9000 -P --name ex nginx:alpine; shipyard port ex`. Expect: 80 and 9000 published on free ports.
- [ ] `shipyard run -d --rm --name gone alpine sleep 2; sleep 4; shipyard ps -a | grep gone`. Expect: nothing (removed once it ended).
- [ ] `shipyard run -d --rm --restart always alpine true; echo $?`. Expect: refused (`--rm` with a policy).
- [ ] `shipyard run -d alpine nosuchcmd; shipyard ps -a`. Expect: `run -d` fails; the container is left `Created` with no last end (as Docker does).
- [ ] Ctrl-C during `shipyard run -d --name cc postgres:16` while it pulls. Expect: everything undone, no `cc` in `ps -a`.

### Names, IDs, serials

- [ ] `shipyard inspect --shipyard web | grep serial`. Expect: an ISO 6346 serial like `SHPU 304812 2`.
- [ ] `shipyard logs "SHPU 304812 2"` (your serial), with and without spaces and check digit. Expect: works.
- [ ] `shipyard logs <first 4 chars of web's ID>`. Expect: works if only one ID starts that way.
- [ ] `shipyard run -d --name "$(printf 'a%.0s' {1..64})" alpine true; echo $?`. Expect: refused (63 characters at most).

### `ps`

- [ ] `shipyard ps`. Expect: `CONTAINER ID   IMAGE   COMMAND   CREATED   STATUS   PORTS   NAMES`, only those up, newest first; `web` shows `0.0.0.0:8080->80/tcp`.
- [ ] `shipyard ps -a`, `-q`, `--no-trunc`, `-n 2`, `-l`, `-s`. Expect: all; IDs only; whole IDs and commands; two newest; the newest; a `Size` of `0B`.
- [ ] `shipyard ps -f name=^web$`. Expect: only `web`.
- [ ] `shipyard ps -f 'name=we*'; echo $?`. Expect: refused (`*` isn't taken), exit 1.
- [ ] `shipyard ps -f status=exited`, `-f exited=0`, `-f ancestor=alpine`, `-f label=tier`, `-f label=tier=web`, `-f health=none`, `-f volume=<name>`, `-f network=bridge`, `-f before=web`, `-f since=web`. Expect: each matches as Docker's.
- [ ] `shipyard ps -f ' Status = exited'`. Expect: accepted (any case, spaces).
- [ ] `shipyard ps -f nosuchkey=1; echo $?`. Expect: refused, 1. `shipyard ps -f novalue; echo $?`: 125.
- [ ] `shipyard ps --format json`. Expect: one compact JSON object per line.
- [ ] `shipyard ps --format 'table {{.Names}}\t{{.Status}}'`. Expect: aligned, with a header.
- [ ] `shipyard ps --format '{{.Names}} {{.Image}} {{.Label "tier"}}'`. Expect: a line per container.
- [ ] `shipyard ps --format '{{.Nope}}'; echo $?`. Expect: fails, 1.
- [ ] `shipyard ps -q --format '{{.Names}}'`. Expect: IDs, and a warning the format is ignored.
- [ ] `shipyard ps --format '{{pad .Names 3 3}}|{{upper .Names}}|{{truncate .Names 2}}'`. Expect: Docker's functions work.

### `inspect`

- [ ] `shipyard inspect web`. Expect: a JSON array, 4-space indent, Docker's shape: `Id`, `Name` `/web`, `State`, `Config`, `HostConfig`, `NetworkSettings.Ports`, `GraphDriver` `overlay`.
- [ ] `shipyard inspect -f '{{.State.Status}} {{.Config.Image}}' web`. Expect: `running nginx:alpine`.
- [ ] `shipyard inspect -f json web`. Expect: one compact array.
- [ ] `shipyard inspect --shipyard web`. Expect: `id`, `serial`, `name`, `created`, `image`, `image_id`, `state`, `ports`, `labels`, `auto_remove`, `restart_count`, `log_dir`, `config`.
- [ ] `shipyard inspect web nosuch alpine; echo $?`. Expect: `web` and the `alpine` image printed, `no such object: nosuch` on stderr, exit 1.
- [ ] `shipyard inspect --type volume <a volume>`, `--type network bridge`, `--type image alpine`. Expect: only that kind.
- [ ] `shipyard inspect -f '{{.State.Pid}}' web`. Expect: a host PID (0 once stopped).
- [ ] `shipyard inspect -f '{{.ResolvConfPath}}' web`; then stop `web` and again. Expect: a path while up, empty once ended.

### `logs`

- [ ] `shipyard run -d --name talk alpine sh -c 'for i in $(seq 1 30); do echo line $i; echo err $i >&2; sleep 1; done'`.
- [ ] `shipyard logs talk`. Expect: everything so far, stdout to stdout, stderr to stderr (`2>/dev/null` hides the `err` lines).
- [ ] `shipyard logs -n 5 talk` (`--tail 5`). Expect: last 5 lines. `-n all`: every line.
- [ ] `shipyard logs -f talk`. Expect: follows, ends by itself when the container stops.
- [ ] `shipyard logs -f talk | head -n1`. Expect: one line, then it ends.
- [ ] `shipyard logs -t talk | head -2`. Expect: each line starts with an RFC 3339 time.
- [ ] `shipyard logs --since 10s talk`, `--until 5s talk`, `--since 2026-10-28T00:00:00Z talk`. Expect: the right windows.
- [ ] After `talk` ends, `shipyard start talk; sleep 3; shipyard logs talk | grep -c 'line 1$'`. Expect: 2 (every start's logs, oldest first).

### `attach`, `start -a`

- [ ] `shipyard run -d -it --name sh1 alpine sh; shipyard attach sh1`, type `echo hi`, then Ctrl-P Ctrl-Q; `echo $?`. Expect: `hi`; exit 0 on detach.
- [ ] `shipyard attach --no-stdin sh1`. Expect: output only, typing doesn't reach it.
- [ ] `shipyard attach sh1`, then `exit`; `echo $?`. Expect: the container's exit code (0).
- [ ] `shipyard attach nosuch; echo $?` and attach to a stopped container. Expect: 1.
- [ ] `shipyard create --name once alpine sh -c 'echo first-byte'; shipyard start -a once`. Expect: `first-byte` (nothing missed).
- [ ] `shipyard create -i --name cat1 alpine cat; echo piped | shipyard start -a -i cat1`. Expect: `piped`.
- [ ] Without a terminal: `shipyard run -d --name loop alpine sh -c 'while true; do date; sleep 1; done'; shipyard attach loop`, Ctrl-C. Expect: the attachment ends, the container runs on.

### `exec`

- [ ] `shipyard exec web cat /etc/hostname`. Expect: the hostname.
- [ ] `shipyard exec -it web sh`, then `ps`, `exit`. Expect: a shell with a terminal.
- [ ] `shipyard exec -u nginx -w /usr/share/nginx web sh -c 'id -un; pwd'`. Expect: `nginx`, `/usr/share/nginx`.
- [ ] `shipyard exec -e DEBUG=1 web env | grep DEBUG`; `TOKEN=t shipyard exec -e TOKEN web env | grep TOKEN`. Expect: `DEBUG=1`, `TOKEN=t`.
- [ ] `shipyard exec --env-file app.env web env | grep DB_`. Expect: the file's variables.
- [ ] `shipyard exec web sh -c 'exit 7'; echo $?`. Expect: 7.
- [ ] `shipyard exec web nosuch; echo $?`. Expect: 127. `shipyard exec web /etc; echo $?`: 126.
- [ ] `shipyard exec -d web sh -c 'sleep 30'; echo $?`. Expect: returns at once, 0.
- [ ] `shipyard exec -d -t web sh; echo $?`. Expect: refused (`-d` with `-t`).
- [ ] `shipyard exec --detach-keys ctrl-a web true` and `shipyard exec --privileged web true`. Expect: both refused.
- [ ] `echo hello | shipyard exec -i web cat`. Expect: `hello` (as it comes, not late).
- [ ] `shipyard exec <a stopped container> true; echo $?`. Expect: 1.
- [ ] `shipyard exec web sh -c 'echo $HOME'`. Expect: `/root`.

### `stop`, `kill`, `restart`, `wait`

- [ ] `shipyard stop web; echo $?`. Expect: prints `web` once ended, exit 0; `curl localhost:8080` fails.
- [ ] `shipyard start web` right after. Expect: works, its port back at once.
- [ ] `shipyard run -d --name slow alpine sh -c 'trap "" TERM; sleep 300'; time shipyard stop -t 2 slow`. Expect: about 2 s (SIGKILL after the timeout); `wait`/`inspect` says 137.
- [ ] `shipyard run -d --name slow2 --stop-timeout 3 alpine sh -c 'trap "" TERM; sleep 300'; time shipyard stop slow2`. Expect: about 3 s.
- [ ] `shipyard run -d --name st --stop-signal SIGUSR1 alpine sleep 300; shipyard stop st; shipyard wait st`. Expect: SIGUSR1 ends it (138).
- [ ] `shipyard stop -s SIGINT slow3` on a fresh sleeper. Expect: 130.
- [ ] `shipyard stop -t -1 <a TERM-ignoring container>` in one terminal, `shipyard kill` it from another. Expect: the stop waits until the kill.
- [ ] `shipyard stop` killed (Ctrl-C) while waiting on a TERM-ignoring container with `-t 10`. Expect: the SIGKILL still comes at the timeout.
- [ ] `shipyard kill web; echo $?`. Expect: SIGKILL at once, prints `web`, exit 0.
- [ ] `shipyard kill -s HUP <running>`. Expect: HUP sent; returns at once.
- [ ] `shipyard kill <stopped container>; echo $?`. Expect: an error, 1.
- [ ] `shipyard restart web`. Expect: stopped then started; one not running is started.
- [ ] `shipyard wait <container that exits 3>`. Expect: `3`. On a container never started: `0`. After `kill`: `137`.
- [ ] `shipyard run -d --rm --name w1 alpine sh -c 'sleep 2; exit 4'; shipyard wait w1`. Expect: `4` though the container goes.
- [ ] `shipyard stop a b nosuch; echo $?`. Expect: `a` and `b` stopped, `nosuch` said, exit 1.
- [ ] `shipyard stop a b c | head -1; echo $?`. Expect: quiet, 0.

### `pause`, `unpause`

- [ ] `shipyard pause web; shipyard ps`. Expect: `Up … (Paused)`; `curl --max-time 2 localhost:8080` hangs/times out.
- [ ] `shipyard exec web true; echo $?`. Expect: refused while paused.
- [ ] `shipyard pause web; echo $?`. Expect: already paused, exit 1.
- [ ] `shipyard unpause web`; `curl localhost:8080`. Expect: answers again.
- [ ] `shipyard unpause web; echo $?`. Expect: not paused, exit 1.
- [ ] `shipyard pause web; shipyard stop -t 2 web`. Expect: a stop thaws it first; ends quickly.
- [ ] `shipyard events --since 2m --until 0s -f event=pause -f event=unpause`. Expect: both events.

### `rename`

- [ ] `shipyard rename web site; shipyard ps`. Expect: prints nothing; `site` listed.
- [ ] `shipyard rename site db; echo $?`. Expect: refused (taken), 1.
- [ ] `shipyard rename site web` back.

### `update`

- [ ] `shipyard update -m 512m --cpus 1.5 --pids-limit 200 web`. Expect: prints `web`; `inspect -f '{{.HostConfig.Memory}} {{.HostConfig.NanoCpus}} {{.HostConfig.PidsLimit}}' web` shows them.
- [ ] `shipyard exec web cat /sys/fs/cgroup/memory.max`. Expect: `536870912`.
- [ ] `shipyard update --restart on-failure:3 web`. Expect: `inspect`'s `RestartPolicy` changes.
- [ ] `shipyard update web; echo $?`. Expect: refused (no flag), 1.
- [ ] `shipyard update --pids-limit -1 web; echo $?`. Expect: refused (lifting isn't supported yet).
- [ ] `shipyard update -m 1m web; echo $?`. Expect: refused by the kernel/limit (at least 6 MiB), record put back.
- [ ] `shipyard update --restart always <a --rm container>`. Expect: refused.

### `top`, `stats`

- [ ] `shipyard top web`. Expect: `UID PID PPID C STIME TTY TIME CMD`, host PIDs, nginx's processes.
- [ ] `shipyard top web -o pid; echo $?`. Expect: refused, 1. `shipyard top web -ef`: works.
- [ ] `shipyard top <stopped>; echo $?`. Expect: 1.
- [ ] `shipyard stats`. Expect: refreshes every second, screen cleared, `CPU %`, `MEM USAGE / LIMIT`, `MEM %`, `NET I/O`, `BLOCK I/O`, `PIDS`; Ctrl-C ends it.
- [ ] `shipyard stats --no-stream web db`. Expect: one table, then exits.
- [ ] `shipyard stats --no-stream --format '{{.Name}} {{.CPUPerc}} {{.MemUsage}}'`. Expect: a line each.
- [ ] `shipyard stats --no-stream --format json`, `--no-trunc`, `-a`. Expect: JSON lines; whole IDs; stopped ones as zeros.
- [ ] `shipyard stats --no-stream nosuch; echo $?`. Expect: said, left out, 1.
- [ ] `shipyard run -d --name burn alpine sh -c 'while :; do :; done'; shipyard stats --no-stream burn`. Expect: CPU near 100%. Then `shipyard rm -f burn`.

### `port`

- [ ] `shipyard port web`. Expect: `80/tcp -> 0.0.0.0:8080` (and `[::]:8080` where IPv6 reaches it).
- [ ] `shipyard port web 80`. Expect: `0.0.0.0:8080`.
- [ ] `shipyard port web 81; echo $?`. Expect: "no public port '81/tcp' published for web", 1.
- [ ] `shipyard port <stopped container>; echo $?`. Expect: prints nothing.
- [ ] `shipyard run -d -p 80 --name rnd nginx:alpine`. Expect: a line `shipyard: published 0.0.0.0:NNNNN -> 80/tcp`; `shipyard port rnd` agrees.
- [ ] **[Linux / Mac VM]** `shipyard port --sockets web` while `curl localhost:8080` loops. Expect: `PROTO LOCAL REMOTE STATE PEER`, `LISTEN` lines first. `--format json`: `protocol`, `local`, `remote`, `state`, `peer`.
- [ ] `shipyard port --sockets <stopped>; echo $?`. Expect: 1.

### `cp`

- [ ] `shipyard cp web:/etc/nginx/nginx.conf ./nginx.conf`. Expect: the file here.
- [ ] `mkdir site && echo hello > site/index.html; shipyard cp ./site/. web:/usr/share/nginx/html; curl localhost:8080`. Expect: `hello`.
- [ ] `shipyard cp web:/var/log - | tar -t`. Expect: a tar listing.
- [ ] `shipyard cp web:/var/log -` with stdout a terminal. Expect: refused (never to a terminal).
- [ ] `tar -c -C site . | shipyard cp - web:/tmp`. Expect: extracted in `/tmp`.
- [ ] `shipyard stop web; shipyard cp ./nginx.conf web:/tmp/copied.conf; shipyard start web; shipyard exec web ls /tmp/copied.conf`. Expect: there (cp works on a stopped container).
- [ ] `shipyard create --name fresh alpine true; shipyard cp fresh:/etc/os-release .`. Expect: works on a never-started container.
- [ ] `shipyard cp -a …` (owners kept) and `shipyard cp -L web:/<a symlink> .` (followed). Expect: as described.
- [ ] `shipyard cp web:/nonexistent .; echo $?`. Expect: an error, 1.
- [ ] `shipyard run -d --read-only --name ro nginx:alpine; shipyard cp ./nginx.conf ro:/x; echo $?`. Expect: refused (read-only root).

### `export`

- [ ] `shipyard export web > web.tar; tar -tf web.tar | head`. Expect: `bin/…` names from its root.
- [ ] `shipyard export -o web2.tar web; ls -l web2.tar`. Expect: mode `-rw-------` (0600).
- [ ] `shipyard export web` to a terminal. Expect: refused.
- [ ] `shipyard run -d --name vol1 -v data1:/data alpine sh -c 'echo x > /data/f; sleep 300'; shipyard export vol1 | tar -t | grep '^data/'`. Expect: `data/` without `f` (never its volumes).
- [ ] `shipyard export web | shipyard import - web-snapshot:1; shipyard run --rm web-snapshot:1 ls /usr/share/nginx/html`. Expect: the image made, your `index.html` there.
- [ ] `shipyard export -o /dev/null web` **[Linux root]**. Expect: writes into `/dev/null`, never replaces it (`ls -l /dev/null` still a character device).

### Sharing namespaces

- [ ] `shipyard run --rm --network container:web alpine wget -qO- localhost | head -3`. Expect: nginx's page.
- [ ] `shipyard run --rm --network container:web -p 9999:80 alpine true; echo $?`. Expect: refused (nothing of its own in that network).
- [ ] `shipyard run --rm --network container:<stopped> alpine true`. Expect: `cannot join network of a non running container: NAME`.
- [ ] `shipyard run --rm --ipc container:db alpine ls /dev/shm`. Expect: works (postgres's shared memory).
- [ ] `shipyard run --rm --ipc host alpine true; echo $?`. Expect: refused.
- [ ] `shipyard stop db` while a container joins it. Expect: `stop` says who shares its namespaces.
- [ ] `shipyard inspect -f '{{.HostConfig.NetworkMode}} {{.HostConfig.IpcMode}}' <joiner>`. Expect: `container:ID`.

### `rm` and prune

- [ ] `shipyard rm web; echo $?`. Expect: refused (running) without `-f`, 1.
- [ ] `shipyard rm -f web`. Expect: prints `web`; gone.
- [ ] `shipyard run -d --name anon -v /data alpine sleep 300; shipyard rm -f -v anon; shipyard volume ls`. Expect: its anonymous volume gone too.
- [ ] `shipyard rm a nosuch b; echo $?`. Expect: `a`, `b` removed, `nosuch` said, 1.
- [ ] `shipyard container prune` with stdin not a terminal (`< /dev/null`). Expect: refused, 1.
- [ ] `shipyard container prune`, answer `n`. Expect: nothing removed.
- [ ] `shipyard container prune -f`. Expect: `Deleted Containers:` with IDs, `Total reclaimed space:`; running and paused ones kept.
- [ ] `shipyard container prune -f --filter until=24h`, `--filter label=tier`, `--filter label!=tier`. Expect: only matching ones.
- [ ] Cleanup: `shipyard rm -f $(shipyard ps -aq)`.

## 4. Resources: memory, CPU, processes

- [ ] `shipyard run --rm -m 64m alpine cat /sys/fs/cgroup/memory.max`. Expect: `67108864`.
- [ ] `shipyard run --rm -m 1m alpine true; echo $?`. Expect: refused (at least 6 MiB), 125.
- [ ] `shipyard run --rm -m 32m alpine sh -c 'x=$(head -c 200m /dev/zero | tr "\0" a); echo done'; echo $?`. Expect: 137 and "the command was killed: out of memory".
- [ ] `shipyard ps -a` / `inspect -f '{{.State.OOMKilled}}'` on an OOM-killed container. Expect: `true`; `events` shows `oom` then `die`.
- [ ] `shipyard run --rm -m 64m alpine cat /sys/fs/cgroup/memory.swap.max`. Expect: `67108864` (swap as much as memory), or a warning where swap isn't accounted.
- [ ] `shipyard run --rm -m 64m --memory-swap 64m alpine cat /sys/fs/cgroup/memory.swap.max`. Expect: `0`.
- [ ] `shipyard run --rm --memory-swap 64m alpine true; echo $?`. Expect: refused (needs `--memory`).
- [ ] `shipyard run --rm --memory-reservation 32m alpine cat /sys/fs/cgroup/memory.low`. Expect: `33554432`.
- [ ] `shipyard run --rm --cpus 0.5 alpine cat /sys/fs/cgroup/cpu.max`. Expect: `50000 100000`.
- [ ] `shipyard run --rm --cpus 1 --cpu-quota 50000 alpine true; echo $?`. Expect: refused (not both).
- [ ] `shipyard run --rm -c 512 alpine cat /sys/fs/cgroup/cpu.weight`. Expect: a lower weight than the default.
- [ ] `shipyard run --rm --cpuset-cpus 0 alpine cat /sys/fs/cgroup/cpuset.cpus` **[root / VM]**. Expect: `0`. **[rootless]**: refused, "it wasn't delegated to this user" unless delegated.
- [ ] `shipyard run --rm alpine cat /sys/fs/cgroup/pids.max`. Expect: `2048` (the default).
- [ ] `shipyard run --rm --pids-limit 50 alpine cat /sys/fs/cgroup/pids.max`. Expect: `50`.
- [ ] `shipyard run --rm --pids-limit -1 alpine cat /sys/fs/cgroup/pids.max`. Expect: `max`.
- [ ] `shipyard run --rm --pids-limit 1 alpine true; echo $?`. Expect: fails, saying the limit is 1.
- [ ] `shipyard run --rm --pids-limit 20 alpine sh -c 'for i in $(seq 40); do sleep 30 & done; wait'`. Expect: `fork` fails past the limit ("can't fork" / resource unavailable).
- [ ] `shipyard run --rm --memory 0 --cpus 0 alpine true`. Expect: 0 means "not set", works.
- [ ] `shipyard run --rm --blkio-weight 500 alpine true`. Expect: works, or refused naming `io.weight` (kernel without it), or a warning that nothing acts on it.
- [ ] **[Linux][rootless, SSH without a user scope]** `shipyard run --rm -m 64m alpine true; echo $?`. Expect: refused (125) with the hint `systemd-run --user --scope shipyard run …`.

## 5. Restart policies (and recovery after a reboot)

- [ ] `shipyard run -d --name rs --restart on-failure:3 alpine sh -c 'sleep 1; exit 1'`; watch `shipyard ps -a` for 30 s. Expect: `Restarting (1) …` between runs; it stops after 3 restarts; `inspect -f '{{.RestartCount}}' rs` is 3.
- [ ] `shipyard run -d --name ra --restart always alpine sh -c 'sleep 2; exit 0'`. Expect: keeps coming back.
- [ ] `shipyard stop ra`; wait 10 s. Expect: stays stopped (never after you stop it).
- [ ] `shipyard rm ra` while it's `Restarting`. Expect: refused (counts as up); `rm -f` works.
- [ ] `shipyard run -d --name ru --restart unless-stopped nginx:alpine; shipyard update --restart no ru; shipyard inspect -f '{{.HostConfig.RestartPolicy.Name}}' ru`. Expect: `no`.
- [ ] `shipyard logs ra` after several restarts. Expect: every start's logs (at most the last 10 starts kept).
- [ ] `shipyard run --restart on-failure alpine sh -c 'exit 2'; echo $?`. Expect: `run` returns 2 at the first end; the container goes on restarting in the background.
- [ ] `shipyard events --since 2m --until 0s -f container=rs -f event=start --format '{{.Actor.Attributes.restartCount}}'`. Expect: the restart counts.
- [ ] **[Linux]** Reboot the host with containers `always`, `unless-stopped` (one you stopped), `on-failure` (ended 1) and `no`. Before `system recover`: `shipyard ps -a` shows them `Exited (?)`.
- [ ] **[Linux]** `shipyard system recover`. Expect: prints the names it started: `always` (even stopped), `unless-stopped` (unless stopped), `on-failure` (ended with a failure); `no` left.
- [ ] **[Linux]** With the systemd units in `contrib/systemd` installed (`shipyard-recover.service`, `shipyard-stop.service`), reboot. Expect: containers come back with no command typed; `shipyard doctor` says the units are enabled.
- [ ] **[Linux]** `shipyard system stop`. Expect: every container up is stopped at once; `shipyard doctor` then warns restarts are held off until `system recover`.
- [ ] **[Linux]** `sudo kill -9 <a container's __shipyard-monitor PID>`; then `shipyard ps -a`. Expect: `Exited (?)`; the next `shipyard run`/`start` says `warning: container NAME ended while nothing recorded its end …` and settles it; `wait` gives 125.
- [ ] **[Mac]** A container with `--restart always` running; `shipyard vm stop`; then `shipyard ps`. Expect: the VM starts again and brings the container back.

## 6. Health checks

- [ ] `shipyard run -d --name hc --health-cmd 'wget -qO- http://localhost/ || exit 1' --health-interval 2s nginx:alpine; shipyard ps`. Expect: `(health: starting)`, then `(healthy)` within seconds.
- [ ] `shipyard inspect -f '{{json .State.Health}}' hc`. Expect: `Status`, `FailingStreak`, `Log` (latest five checks).
- [ ] `shipyard exec hc rm /usr/share/nginx/html/index.html`; wait ~10 s. Expect: `(unhealthy)` after 3 failures; `events -f event=health_status` shows the changes.
- [ ] `shipyard run -d --name hc2 --health-cmd 'exit 1' --health-interval 1s --health-retries 2 --health-start-period 5s alpine sleep 300`. Expect: stays `starting` during the start period, then `unhealthy`.
- [ ] `shipyard run -d --name hc3 --health-cmd 'sleep 10' --health-timeout 1s --health-interval 2s alpine sleep 300`; `inspect` its log. Expect: `Health check exceeded timeout (1s)`.
- [ ] `shipyard run -d --health-cmd 'nosuchcmd' --health-interval 1s --name hc4 alpine sleep 300`. Expect: failures with exit 127.
- [ ] `shipyard run -d --no-healthcheck --health-interval 1s alpine true; echo $?`. Expect: refused (`--no-healthcheck` with a `--health-*` flag).
- [ ] `shipyard run -d --health-interval 5s alpine sleep 60`. Expect: a warning that there's no check to change; made anyway.
- [ ] `shipyard run -d --health-cmd '' alpine true; echo $?`. Expect: refused (empty).
- [ ] `shipyard run -d --health-interval 0s --health-cmd true alpine true; echo $?`. Expect: refused (never zero).
- [ ] `shipyard ps -f health=healthy`, `-f health=unhealthy`. Expect: matches.
- [ ] `shipyard pause hc; sleep 5; shipyard unpause hc`. Expect: no checks while paused; they go on after.
- [ ] `shipyard stop hc; shipyard inspect -f '{{.State.Health.Status}}' hc`. Expect: `unhealthy` (ended).
- [ ] `ps -ef | grep __health` on the host (Linux; on the Mac: `shipyard top` won't show it). Expect: a `__health` companion while a checked container runs.
- [ ] Cleanup: `shipyard rm -f hc hc2 hc3 hc4`.

## 7. Events

- [ ] `shipyard events` in one terminal; in another: `shipyard run --rm --name ev alpine sh -c 'exit 3'`. Expect: `container create`, `start`, `die (exitCode=3, image=alpine, name=ev)`, `destroy`, RFC 3339 UTC times.
- [ ] `shipyard events --since 1h --until 0s`. Expect: the last hour, then exits.
- [ ] `shipyard events --until 0s`. Expect: everything kept, then exits.
- [ ] `shipyard events --since 1h` (no `--until`). Expect: the past hour, then waits.
- [ ] `shipyard events -f container=ev -f event=die --since 1h --until 0s`. Expect: only that.
- [ ] `shipyard events -f type=volume --since 1h --until 0s` after `volume create`/`rm`. Expect: `volume create`, `volume destroy`.
- [ ] `shipyard events -f image=alpine --since 1h --until 0s`. Expect: pulls, and containers made from it.
- [ ] `shipyard events -f label=tier=web --since 1h --until 0s`. Expect: that container's events.
- [ ] `shipyard events --since 3d --until 0s --format json | head -1`. Expect: Docker's message (`status`, `id`, `from`, `Type`, `Action`, `Actor`, `scope`, `time`, `timeNano`).
- [ ] `shipyard events --since 1h --until 0s --format '{{.Type}} {{.Action}} {{.Actor.Attributes.name}}'`. Expect: a line each; `<no value>` where an attribute is missing.
- [ ] `shipyard events --since 0s --until 1h; echo $?` (until an hour ago, before since). Expect: refused, 1.
- [ ] `shipyard events -f daemon=x; echo $?`. Expect: refused, 1.
- [ ] `shipyard events --since 1h --until 0s | head -n 2; echo $?`. Expect: quiet, 0.
- [ ] Events for `kill` (`signal=SIGKILL`), `rename` (`oldName`), `update` (`restartPolicy`), `pause`, `unpause`, `network connect`/`disconnect` (`container`), `image pull`/`untag`/`delete`. Expect: each appears.

## 8. Images

### Pull and list

- [ ] `shipyard pull alpine:3.20`. Expect: each layer, then `Digest: sha256:…`.
- [ ] `shipyard pull -q alpine:3.20`. Expect: only the digest.
- [ ] `shipyard pull alpine@sha256:<a digest>`. Expect: exactly that image.
- [ ] `shipyard pull ghcr.io/<some public image>`; `shipyard pull quay.io/<public>`. Expect: works.
- [ ] `shipyard pull nosuch/really-not-there:1; echo $?`. Expect: an error, 1.
- [ ] Pull a large image and Ctrl-C half way; pull again. Expect: goes on where it stopped (only the rest asked for).
- [ ] `shipyard images`. Expect: `REPOSITORY TAG IMAGE ID CREATED SIZE`, a row per name, newest first, `SIZE` compressed.
- [ ] `shipyard images alpine`, `shipyard images 'alpine:3.*'`, `-q`, `--digests`, `--no-trunc`, `-a`. Expect: as Docker's.
- [ ] `shipyard images -f reference='alp*'`, `-f dangling=true`, `-f label=x`. Expect: filtered.
- [ ] `shipyard images --format '{{.Repository}}:{{.Tag}} {{.Size}} {{.Containers}}'`. Expect: `Containers` is `N/A`.

### Tag, inspect, history, remove

- [ ] `shipyard tag alpine:3.20 base`; `shipyard images`. Expect: two names, same ID.
- [ ] `shipyard tag alpine:3.20 base@sha256:abc; echo $?`. Expect: refused (no digest as target).
- [ ] `shipyard image inspect base`. Expect: Docker's `docker image inspect` JSON array. `-f '{{.Id}}'`: the manifest digest.
- [ ] `shipyard image inspect nosuch; echo $?`. Expect: said, 1.
- [ ] `shipyard history alpine:3.20`. Expect: `IMAGE CREATED CREATED BY SIZE COMMENT`, newest first, the image's ID on the top row, `<missing>` below.
- [ ] `shipyard history --no-trunc -H=false -q alpine:3.20` and `--format '{{.CreatedBy}}'`. Expect: as described.
- [ ] `shipyard rmi base`. Expect: `alpine:3.20` stays.
- [ ] `shipyard run -d --name usesit alpine:3.20 sleep 300; shipyard rmi alpine:3.20; echo $?`. Expect: refused, `… is used by container …`, 1.
- [ ] `shipyard rmi -f alpine:3.20`. Expect: untagged; `shipyard restart usesit` still works.
- [ ] `shipyard rmi <first 4 hex of an ID>`. Expect: every reference of it removed.
- [ ] `shipyard rmi a nosuch; echo $?`. Expect: `a` removed, `nosuch` said, 1.

### Save and load

- [ ] `shipyard save -o images.tar alpine:3.20 nginx:alpine; ls -l images.tar; tar -tf images.tar | head`. Expect: mode 0600; `oci-layout`, `index.json`, `manifest.json`, `blobs/sha256/…`.
- [ ] `shipyard save alpine | gzip > alpine.tar.gz`. Expect: every alpine tag.
- [ ] `shipyard save alpine:3.20` to a terminal. Expect: refused.
- [ ] `shipyard save -o images2.tar alpine:3.20; cmp images.tar images2.tar` with the same image list. Expect: the same images give the same bytes.
- [ ] `shipyard rmi alpine:3.20 nginx:alpine; shipyard load -i images.tar`. Expect: `Loaded image: alpine:3.20`, `Loaded image: nginx:alpine`.
- [ ] `gunzip -c alpine.tar.gz | shipyard load`. Expect: loads.
- [ ] Load an archive whose name exists for another image. Expect: `The image … already exists, renaming the old one with ID sha256:… to empty string`; `-q` leaves that out.
- [ ] `head -c 100000 images.tar | shipyard load; echo $?`. Expect: nothing loaded, an error.
- [ ] `shipyard save -o byid.tar <image ID>; shipyard load -i byid.tar`. Expect: an image with no name isn't loaded, said.
- [ ] Docker interop: `docker save` from a real Docker (if you have one), then `shipyard load`; and `shipyard save … | docker load`. Expect: both work.
- [ ] `xz -c images.tar > i.tar.xz; shipyard load -i i.tar.xz; echo $?`. Expect: refused by name.

### Import

- [ ] `shipyard import -c 'CMD ["/bin/sh","-c","echo imported"]' -c 'ENV MODE=prod' -m 'by hand' web.tar app:1; shipyard run --rm app:1`. Expect: its ID printed; `imported`.
- [ ] `shipyard history app:1`. Expect: the comment `by hand`.
- [ ] `shipyard import web.tar; echo $?`. Expect: refused (a name is required).
- [ ] `shipyard import https://example.com/x.tar app:2; echo $?`. Expect: refused (a URL isn't fetched).
- [ ] `shipyard import -c 'HEALTHCHECK CMD true' web.tar app:3; echo $?`. Expect: refused.
- [ ] `shipyard import images.tar app:4`. Expect: a warning that it's an image archive (`load` reads those).
- [ ] `: > empty.tar; shipyard import empty.tar app:5; echo $?`. Expect: nothing made.

### Prune, disk use

- [ ] `shipyard image prune` (answer `n`), then `-f`. Expect: Docker's warning first; with `-f`: `Deleted Images:` and `Total reclaimed space:`.
- [ ] `shipyard image prune -a -f`. Expect: every image no container was made from removed; a container's image kept.
- [ ] `shipyard image prune -f --filter until=1h`, `--filter label=x`. Expect: filtered.
- [ ] `shipyard image prune < /dev/null; echo $?`. Expect: refused without a terminal.
- [ ] `shipyard system df`. Expect: `TYPE TOTAL ACTIVE SIZE RECLAIMABLE`, rows `Images`, `Containers`, `Local Volumes`, `Build Cache`.
- [ ] `shipyard system df --format '{{.Type}} {{.Size}}'` and `--format json`. Expect: as asked.

### Platforms, Rosetta

- [ ] **[Mac]** `shipyard doctor | grep rosetta`. Expect: `installed` (or how to install: `softwareupdate --install-rosetta --agree-to-license`, then `vm stop`).
- [ ] **[Mac]** `shipyard run --rm --platform linux/amd64 alpine uname -m`. Expect: `x86_64`.
- [ ] **[Mac]** `shipyard pull --platform linux/amd64 alpine; shipyard run --rm alpine uname -m`. Expect: `x86_64`, saying it runs through Rosetta; `shipyard pull alpine` takes it back to arm64.
- [ ] **[Mac]** Pull an amd64-only image (an old x86-only one). Expect: pulled as amd64, with a warning; runs.
- [ ] `shipyard pull --platform linux/s390x alpine; echo $?`. Expect: refused before anything is pulled, saying what runs here.
- [ ] **[Linux]** `shipyard pull --platform linux/amd64 alpine` on an arm64 host (or the reverse). Expect: refused (only its own platform runs).
- [ ] **[Mac]** `docker run --rm --platform linux/amd64 alpine uname -m` through the API. Expect: `x86_64`.

### Store and overlay

- [ ] **[Linux]** `SHIPYARD_IMAGE_STORE=relative/path shipyard images; echo $?`. Expect: refused (must be absolute).
- [ ] **[Linux]** `SHIPYARD_IMAGE_STORE=/tmp/sy-store shipyard run --rm alpine true`. Expect: works in that store.
- [ ] **[Linux][rootless]** `ls -ln ~/.local/share/shipyard/images/layers | head`. Expect: files owned by your subordinate IDs.

## 9. Build (BuildKit)

Make a project in your home: `mkdir -p ~/sy-test/app && cd ~/sy-test/app`, with
```Dockerfile
FROM alpine:3.20
ARG VERSION=1
RUN echo "v$VERSION" > /version
COPY hello.txt /hello.txt
CMD ["cat", "/version", "/hello.txt"]
```
and `echo hi > hello.txt`.

- [ ] `shipyard build -t app:1 .`. Expect: BuildKit's progress (the first build pulls `moby/buildkit:v0.34.1` by digest), a builder container `shipyard-build-…` while it runs and gone at the end; `app:1` in `shipyard images`.
- [ ] `shipyard run --rm app:1`. Expect: `v1`, `hi`.
- [ ] `shipyard build -t app:2 --build-arg VERSION=2 . && shipyard run --rm app:2`. Expect: `v2`.
- [ ] `shipyard build -q -t app:3 .`. Expect: only the image's ID.
- [ ] `shipyard build -t a:1 -t a:2 .`. Expect: both names.
- [ ] `shipyard build .; echo $?`. Expect: refused (a name with `-t` is required).
- [ ] Rebuild without changes. Expect: cached steps (fast).
- [ ] `shipyard build --no-cache -t app:1 .`. Expect: every step runs again.
- [ ] `shipyard build --pull -t app:1 .`. Expect: the base asked for again.
- [ ] `shipyard build --progress plain -t app:1 .` and `--progress quiet`. Expect: plain lines; none.
- [ ] `shipyard build --label org.x=y -t app:1 .; shipyard image inspect -f '{{.Config.Labels}}' app:1`. Expect: the label.
- [ ] A multi-stage Dockerfile with `FROM alpine AS builder … FROM alpine COPY --from=builder …` and `--target builder`. Expect: builds the stage asked for.
- [ ] `RUN --mount=type=cache,target=/var/cache/apk apk add curl` twice. Expect: the second reuses the cache.
- [ ] `echo token > ~/sy-test/secret.txt; shipyard build --secret id=npm,src=$HOME/sy-test/secret.txt -t s:1 .` with `RUN --mount=type=secret,id=npm cat /run/secrets/npm`. Expect: the secret reaches the step; `shipyard history --no-trunc s:1` and the image's layers don't hold it.
- [ ] `shipyard build -f ci/Dockerfile -t app:1 .` (a Dockerfile elsewhere in the context) and `shipyard build -f - -t app:1 . < Dockerfile`. Expect: both build.
- [ ] `shipyard build -o type=local,dest=out .` (or `-o out`). Expect: the build's files in `out/`, owned by you; no image.
- [ ] `shipyard build -o type=tar,dest=out.tar .; ls -l out.tar`. Expect: mode 0600. `-o type=tar,dest=- . | tar -t | head`: works.
- [ ] `shipyard build -t x:1 -o out .; echo $?`. Expect: refused (`-t` and `-o` together).
- [ ] `shipyard build -t base:1 .` then a Dockerfile `FROM base:1`, with the network off (or `--pull` absent). Expect: builds from the store's image, asks no registry.
- [ ] Ctrl-C during a build. Expect: its builder removed.
- [ ] Two builds of the same context at once. Expect: the second waits, says so.
- [ ] A failing `RUN false`. Expect: BuildKit's last lines, exit 1.
- [ ] `shipyard system df`. Expect: a `Build Cache` row.
- [ ] `shipyard builder prune` (asks), then `-f`. Expect: every context's cache no build is using emptied, with what's reclaimed.
- [ ] A context with a symlinked Dockerfile pointing outside it (`ln -s /etc/passwd Dockerfile`). Expect: refused.
- [ ] **[Linux][root]** a context with a file mode 0600 owned by root. Expect: refused by name (must be readable to every user).
- [ ] **[Linux][root]** a user in `/etc/subuid` given part of the 2100000000 range. Expect: a root build is refused.
- [ ] **[Mac]** `cd /tmp && mkdir b && cd b && printf 'FROM alpine\n' > Dockerfile && shipyard build -t t:1 .; echo $?`. Expect: refused, saying the builder can't enter it (build from your home). (F181)
- [ ] **[Mac]** `shipyard build --platform linux/amd64 -t amd:1 ~/sy-test/app && shipyard run --rm amd:1 uname -m`… (add `RUN uname -m` to see it). Expect: `x86_64` steps through Rosetta.
- [ ] `docker build .` through the API. Expect: not served (use `shipyard build`).

## 10. Registries: login, push, logout

Start a local registry (HTTP is used for `localhost`): `shipyard run -d --name reg -p 5000:5000 registry:2`.

- [ ] `shipyard tag app:1 localhost:5000/app:1; shipyard push localhost:5000/app:1`. Expect: a line per blob, then `1: digest: sha256:… size: …`.
- [ ] Push again. Expect: each blob `already there` / `Layer already exists`.
- [ ] `shipyard rmi localhost:5000/app:1; shipyard pull localhost:5000/app:1@sha256:<digest>`. Expect: the same digest and ID.
- [ ] `shipyard tag app:2 localhost:5000/app:2; shipyard push -a localhost:5000/app`. Expect: every tag of the repository.
- [ ] `shipyard push -q localhost:5000/app:1`. Expect: only the name pushed.
- [ ] `shipyard push localhost:5000/nosuch:1; echo $?`. Expect: an error, 1.
- [ ] `shipyard login ghcr.io` (or Docker Hub: `shipyard login`). Expect: asks username and password (no echo; Ctrl-C cancels it and puts the terminal back); `Login Succeeded`.
- [ ] A wrong password. Expect: `unauthorized: …`, nothing kept.
- [ ] `echo "$TOKEN" | shipyard login -u me --password-stdin ghcr.io`. Expect: works.
- [ ] `shipyard login -u me -p pw ghcr.io`. Expect: a warning (others can see `-p`).
- [ ] `cat ~/.docker/config.json`. Expect: kept where `docker login` keeps it: the credential helper (`credsStore`, e.g. `osxkeychain`), else `auths` base64 with a warning it's not encrypted; other keys kept, mode unchanged.
- [ ] `shipyard pull ghcr.io/<you>/<private>:tag`. Expect: signed in, works.
- [ ] **[Mac]** `shipyard push` / `pull` of a private image. Expect: the Mac's credential handed to that one command (no login inside the VM needed).
- [ ] `shipyard push ghcr.io/<someone-else>/x:1` with your login. Expect: `denied: …` (not "sign in again").
- [ ] `shipyard logout ghcr.io`, then again. Expect: removed; then `Not logged in to ghcr.io`.
- [ ] `docker login` then `shipyard pull` of the private image. Expect: works (same config).
- [ ] Cleanup: `shipyard rm -f reg`.

## 11. Networks

On the Mac these run in the VM (as root there). On Linux they're root's: prefix `sudo`.

### Create, list, inspect, remove

- [ ] `shipyard network create app`. Expect: prints `app`.
- [ ] `shipyard network ls`. Expect: `NETWORK ID NAME DRIVER SCOPE` with `app`, `bridge`, `host`, `none`. **[rootless]**: `host`, `none`, `pasta` only.
- [ ] `shipyard network ls -q`, `--no-trunc`, `-f name=ap`, `-f type=custom`, `-f type=builtin`, `-f dangling=true`, `-f driver=bridge`, `--format '{{.Name}} {{.IPv6}} {{.Internal}}'`. Expect: as Docker's.
- [ ] `shipyard network inspect app`. Expect: Docker's shape: `Name`, `Id`, `Driver`, `IPAM` subnet `10.89.x.0/24` and gateway, `Internal`, `Options`, `Labels`, `Containers`.
- [ ] `shipyard network inspect --shipyard app`. Expect: subnet, gateway, `Bridge` (`shy-` + 10 hex), `DNSRunning`, and running containers with `HostVeth`.
- [ ] `shipyard network inspect -f '{{.Name}} {{.Driver}}' app`. Expect: `app bridge`.
- [ ] `shipyard network create --subnet 10.90.0.0/24 --gateway 10.90.0.1 -l team=x custom`. Expect: those values; label shown.
- [ ] `shipyard network create --subnet 10.90.0.0/24 clash; echo $?`. Expect: refused (overlaps).
- [ ] `shipyard network create --gateway 10.90.1.0 --subnet 10.90.1.0/24 bad; echo $?`. Expect: refused (network address).
- [ ] `shipyard network create -d overlay x; echo $?` and `-o com.docker.network.bridge.name=x y`. Expect: refused (bridge only; options at their defaults only).
- [ ] `shipyard network create -o com.shipyard.network.isolate-host=true iso`. Expect: same as `--isolate-host`.
- [ ] `shipyard network create bridge; echo $?`. Expect: refused.
- [ ] `shipyard network create 'bad/name'; echo $?`. Expect: refused.
- [ ] `shipyard network rm app` while a container runs on it. Expect: "the network app is in use by …", exit 1.
- [ ] `shipyard network rm bridge; echo $?`. Expect: refused.
- [ ] Stop the container, then `shipyard network rm app`. Expect: removed; the stopped container's next start fails ("there's no network named app").
- [ ] `shipyard network prune` (asks), `-f`, `--filter until=1h`. Expect: every network no container up is on, but `bridge`.
- [ ] `shipyard network prune < /dev/null; echo $?`. Expect: refused without `-f`.

### Addresses, names and DNS

- [ ] `shipyard network create app; shipyard run -d --network app --name db -e POSTGRES_PASSWORD=x postgres:16; shipyard run --rm --network app alpine nslookup db`. Expect: db's address on `app`.
- [ ] `shipyard run --rm --network app alpine nslookup DB`. Expect: the same (any case).
- [ ] `shipyard run -d --network app --name web --network-alias www nginx:alpine; shipyard run --rm --network app alpine wget -qO- www | head -3`. Expect: nginx's page.
- [ ] `ID=$(shipyard ps -q -f name=^web$); shipyard run --rm --network app alpine ping -c1 $ID`. Expect: answers to its short ID too.
- [ ] `shipyard run -d --network app --name other --network-alias db alpine sleep 60; echo $?`. Expect: refused, "the name db is taken on the network app, by a running container".
- [ ] `shipyard rename web site; shipyard run --rm --network app alpine nslookup site`. Expect: answers within a second; `web` no longer does.
- [ ] `shipyard run --rm --network app alpine nslookup example.com`. Expect: answered (forwarded to the host's resolvers).
- [ ] `shipyard run --rm alpine nslookup db` (default `bridge`). Expect: not found (bridge has no names).
- [ ] `shipyard run --rm --network app --dns 1.1.1.1 alpine nslookup db`. Expect: fails (other resolvers instead of the network's).
- [ ] `shipyard run --rm --network app --ip 10.89.0.50 alpine ip addr show eth0`. Expect: `10.89.0.50`; MAC `02:42:0a:59:00:32`.
- [ ] `shipyard run --rm --network app --mac-address 02:11:22:33:44:55 alpine ip link show eth0`. Expect: that MAC.
- [ ] `shipyard run --rm --network app --ip 192.168.99.9 alpine true; echo $?`. Expect: refused (outside the subnet).
- [ ] `shipyard run --rm --network bridge --ip 10.88.0.50 alpine ip addr show eth0`. Expect: that address. Through the API, `docker run --rm --ip 10.88.0.51 alpine true` (no `--network`): refused, "make a network of your own".
- [ ] `shipyard run --rm --network-alias x alpine true; echo $?`. Expect: refused (needs a network you made).
- [ ] `shipyard run --rm --add-host db2:10.0.0.5 --dns 1.1.1.1 --dns-search corp --dns-option ndots:2 alpine cat /etc/resolv.conf /etc/hosts`. Expect: all of those there.
- [ ] `shipyard run --rm alpine cat /etc/resolv.conf` on a host with systemd-resolved **[Linux]**. Expect: the real upstream servers, never `127.0.0.53`.
- [ ] `shipyard network inspect --shipyard app | grep DNSRunning`, then remove every container on it, wait 30 s. Expect: `true`, then `false`.

### Several networks, connect, disconnect

- [ ] `shipyard network create front; shipyard network create back; shipyard run -d --network front --name fweb nginx:alpine; shipyard run -d --network back --name bdb -e POSTGRES_PASSWORD=x postgres:16; shipyard run --rm --network front --network back alpine sh -c 'wget -qO- fweb | head -1; nc -z bdb 5432 && echo db-ok'`. Expect: both reached.
- [ ] `shipyard run --rm --network front alpine nc -z -w 2 bdb 5432; echo $?`. Expect: can't (different networks).
- [ ] `shipyard run --rm --network name=front,ip=10.89.0.60,alias=w2 --network name=back,alias=c2 alpine ip addr`. Expect: `eth0` on front at `.60`, `eth1` on back.
- [ ] `shipyard run --rm --network front --network front alpine true; echo $?` and `--network host --network front`. Expect: refused.
- [ ] `shipyard network connect back fweb; shipyard exec fweb ip addr show eth1`. Expect: an address on `back` at once.
- [ ] `shipyard network connect --ip 10.89.1.20 --alias api back <another running container>`. Expect: that address and alias.
- [ ] `shipyard network disconnect back fweb`. Expect: `eth1` gone.
- [ ] `shipyard network disconnect front fweb; echo $?`. Expect: refused (its way out / only network); `--force` lets it go where it has another.
- [ ] `shipyard network connect back <stopped container>; shipyard start <it>`. Expect: joins at the next start.
- [ ] `shipyard events --since 5m --until 0s -f event=connect -f event=disconnect`. Expect: both, with the container's ID.
- [ ] **[rootless]** `shipyard network connect …`. Expect: refused (root only).

### Internal, isolate-host, conntrack-limit, IPv6

- [ ] `shipyard network create --internal intra; shipyard run --rm --network intra alpine wget -qO- -T 3 example.com; echo $?`. Expect: fails (no way out); `nslookup example.com` refused too.
- [ ] `shipyard run --rm --network intra -p 8081:80 nginx:alpine; echo $?`. Expect: refused (no ports on an internal network).
- [ ] `shipyard network create --isolate-host iso; shipyard run --rm --network iso alpine wget -qO- -T 3 http://<host's address>:<a host service port>`. Expect: can't reach the host (positive control: the same from `--network app` reaches it); DNS still works.
- [ ] `shipyard run --rm --network iso alpine wget -qO- -T 3 http://169.254.169.254/`. Expect: refused.
- [ ] `shipyard network create --conntrack-limit lim`. Expect: made; `network inspect` shows the option.
- [ ] `shipyard network create --ipv6 v6; shipyard run --rm --network v6 alpine ip -6 addr show eth0`. Expect: a ULA `fd…` address whose end matches the IPv4's.
- [ ] `shipyard network create --ipv6 --subnet 10.91.0.0/24 --subnet fd00:1::/64 v6b`. Expect: that prefix.
- [ ] `shipyard run --rm --network v6 -p '[::1]:8082:80' nginx:alpine; echo $?`. Expect: refused (`[::1]`).

### Published ports

- [ ] `shipyard run -d --name p1 -p 8080:80 nginx:alpine; curl -s localhost:8080 | head -1`. Expect: nginx.
- [ ] `shipyard run -d -p 8080:80 nginx:alpine; echo $?`. Expect: "port 8080/tcp on 0.0.0.0 is already in use".
- [ ] `shipyard run -d --name p2 -p 127.0.0.1:8083:80 nginx:alpine`. Expect: reachable on `127.0.0.1:8083` only.
- [ ] `shipyard run -d --name p3 -p 127.0.0.1::80 nginx:alpine; shipyard port p3`. Expect: a free port on `127.0.0.1`.
- [ ] `shipyard run -d --name p4 -p 8000-8002:80-82 nginx:alpine; shipyard port p4`. Expect: three lines.
- [ ] `shipyard run -d --name p5 -P nginx:alpine`. Expect: `shipyard: published 0.0.0.0:NNNNN -> 80/tcp`.
- [ ] `shipyard rm -f p1; shipyard run -d --name p1b -p 8080:80 nginx:alpine`. Expect: the port is free again at once (even after serving connections).
- [ ] UDP: `shipyard run -d --name u1 -p 5353:53/udp alpine sh -c 'nc -u -l -p 53 -e cat'` (or any UDP echo), then `echo ping | nc -u -w1 127.0.0.1 5353`. Expect: `ping` back.
- [ ] `shipyard run -d -p 8084:80 --network host nginx:alpine; echo $?` and `--network none`. Expect: refused.
- [ ] **[Linux root]** From another machine on your LAN: `curl http://<host>:8080`. Expect: answers (published on every address). On `127.0.0.1:8083`: doesn't.
- [ ] **[Linux root]** From a container: `wget -qO- http://<host's address>:8080`. Expect: answers (hairpin).

### Rootless networking (pasta) **[Linux][rootless]**

- [ ] `shipyard run --rm alpine wget -qO- example.com | head -1`. Expect: works through pasta.
- [ ] `shipyard run --rm alpine cat /etc/resolv.conf`. Expect: `nameserver 169.254.1.1`.
- [ ] `shipyard run -d --name rp -p 8080:80 nginx:alpine; curl localhost:8080`. Expect: answers.
- [ ] `shipyard run -d -p 80:80 nginx:alpine; echo $?`. Expect: refused unless `ip_unprivileged_port_start` allows it.
- [ ] `shipyard run --rm --network bridge alpine true; echo $?`. Expect: "bridge networks need root for now…".
- [ ] Uninstall/rename `pasta` temporarily; `shipyard run --rm alpine true; echo $?`. Expect: refused, "install the passt package", with `--network host` / `--network none`.
- [ ] `pkill -9 pasta` while `rp` runs; `shipyard ps`. Expect: `Up … (no network)`; `inspect --shipyard rp` has `state.network_lost`; `events` has `network_lost`. `shipyard restart rp` gives it a new pasta.

### Other firewalls **[Linux][root]**

- [ ] On Fedora with firewalld: `shipyard run --rm --network app alpine nslookup db`. Expect: a warning naming firewalld, with the `firewall-cmd --zone=trusted --add-interface=shy-…` line; once applied, the warning stops.
- [ ] With Docker installed on the same host: a warning with the three `DOCKER-USER` rules.
- [ ] With ufw enabled: the `ufw route allow …` rules.
- [ ] `sudo nft flush ruleset; sudo shipyard run --no-image true`. Expect: Shipyard's tables come back; published ports work again.
- [ ] `shipyard doctor` as root. Expect: a `firewall` line.
- [ ] Cleanup: `shipyard rm -f $(shipyard ps -aq); shipyard network prune -f`.

## 12. Volumes

- [ ] `shipyard volume create data`. Expect: prints `data`.
- [ ] `shipyard volume create`. Expect: a 64-hex name.
- [ ] `shipyard volume create -l tier=db data2; shipyard volume inspect data2`. Expect: Docker's shape, `Labels` with `tier`, `CreatedAt`, `Mountpoint` `…/volumes/data2/_data`.
- [ ] `shipyard volume create data` again. Expect: left as it is (labels too).
- [ ] `shipyard volume create -o type=nfs x; echo $?` and `-d other x`. Expect: refused.
- [ ] `shipyard volume create 'a/b'; echo $?`. Expect: refused.
- [ ] `shipyard volume ls`, `ls -q`, `-f name=dat`, `-f dangling=true`, `-f label=tier`, `--format '{{.Name}} {{.Mountpoint}}'`. Expect: `DRIVER VOLUME NAME` table; filters work.
- [ ] `shipyard volume inspect -f '{{.Name}}' data`. Expect: `data`.
- [ ] `shipyard run --rm -v data:/var/lib/data alpine sh -c 'echo hi > /var/lib/data/f'; shipyard run --rm -v data:/var/lib/data alpine cat /var/lib/data/f`. Expect: `hi`.
- [ ] `shipyard run --rm -v newvol:/x alpine true; shipyard volume ls`. Expect: `newvol` created on first use; `events` has `volume create`.
- [ ] `shipyard run --rm -v data:/d:ro alpine touch /d/x; echo $?`. Expect: read-only.
- [ ] `shipyard run --rm --mount type=volume,source=data,target=/d alpine ls /d`; `--mount type=bind,source=$PWD,target=/w,readonly alpine ls /w`; `--mount type=tmpfs,target=/c,tmpfs-size=64m alpine df -h /c`. Expect: each works; tmpfs 64M.
- [ ] `shipyard run --rm --mount type=tmpfs,target=/c,tmpfs-size=64MiB alpine true; echo $?`. Expect: refused (`64MiB` not taken).
- [ ] `shipyard run --rm --mount type=bind,source=$PWD,target=/w,bind-propagation=shared alpine true; echo $?`. Expect: refused.
- [ ] `shipyard run --rm --tmpfs /run:size=16m,mode=1777 alpine sh -c 'mount | grep /run'`. Expect: `noexec,nosuid,nodev`, size 16m.
- [ ] `shipyard run --rm --tmpfs /data -v data:/data alpine true; echo $?`. Expect: refused (two mounts at one place).
- [ ] `shipyard run --rm -v /etc/hostname:/x alpine cat /x` **[Linux]**. Expect: a bind of a host file.
- [ ] `shipyard run --rm -v /does/not/exist:/x alpine true; echo $?`. Expect: refused (must exist).
- [ ] `shipyard run --rm -v data:/proc/x alpine true; echo $?` and `-v data:/`. Expect: refused.

### A new volume gets the image's files (copy-up)

- [ ] `shipyard run --rm -v pgdata:/var/lib/postgresql/data postgres:16 ls -la /var/lib/postgresql/data`; then `shipyard run --rm -v nginxconf:/etc/nginx nginx:alpine ls /etc/nginx`. Expect: the second shows the image's files (`nginx.conf`, …) in the new volume, owners kept.
- [ ] `shipyard run --rm -v empty1:/etc/nginx:nocopy nginx:alpine ls /etc/nginx`. Expect: empty.
- [ ] `shipyard run --rm --mount type=volume,source=empty2,target=/etc/nginx,volume-nocopy nginx:alpine ls /etc/nginx`. Expect: empty.
- [ ] A volume with anything in it is never filled again: `shipyard run --rm -v nginxconf:/usr/share/nginx/html nginx:alpine ls /usr/share/nginx/html`. Expect: the earlier files, not the html ones.
- [ ] A bind is never filled: `mkdir emptydir; shipyard run --rm -v $PWD/emptydir:/etc/nginx nginx:alpine true; ls emptydir`. Expect: empty.

### Anonymous volumes

- [ ] `shipyard run -d --name an -v /data alpine sleep 300; shipyard inspect -f '{{json .Mounts}}' an`. Expect: a 64-hex volume labelled `com.docker.volume.anonymous`.
- [ ] `shipyard run -d --name pg -e POSTGRES_PASSWORD=x postgres:16; shipyard inspect -f '{{json .Mounts}}' pg`. Expect: the image's `VOLUME` made anonymous.
- [ ] `shipyard rm -f an; shipyard volume ls`. Expect: its volume kept (plain `rm`).
- [ ] `shipyard rm -f -v pg; shipyard volume ls`. Expect: its anonymous volume gone.
- [ ] `shipyard run --rm -v /data alpine true; shipyard volume ls`. Expect: `--rm` takes its anonymous volumes.

### Removing

- [ ] `shipyard run -d --name holder -v data:/d alpine sleep 300; shipyard volume rm data; echo $?`. Expect: "the volume data is in use by holder", 1.
- [ ] `shipyard stop holder; shipyard volume rm data; echo $?`. Expect: still refused (a stopped container uses it too, per volumes.md).
- [ ] `shipyard rm holder; shipyard volume rm data`. Expect: removed.
- [ ] `shipyard volume rm a nosuch b; echo $?`. Expect: `a`, `b` removed, `nosuch` said, 1.
- [ ] `shipyard volume prune` (answer `n`). Expect: `Remove every volume no container uses, and its data? [y/N]`, then `Nothing removed`.
- [ ] `shipyard volume prune < /dev/null; echo $?`. Expect: refused without `-f`.
- [ ] `shipyard volume prune -f`. Expect: the names removed, `Removed N volume(s)`, and `Kept N in use: …` for those a container names. (Note whether named volumes go too: the CLI says every unused one; `docker volume prune` takes anonymous ones only unless `--all`.)
- [ ] **[Linux][rootless]** `shipyard run --rm -u 1000 -v rv:/d alpine touch /d/f; shipyard volume rm rv`. Expect: removed (subordinate-owned files removed for you).
- [ ] Cleanup: `shipyard rm -f $(shipyard ps -aq); shipyard volume prune -f`.

## 13. Compose

Make `~/sy-test/proj/compose.yaml`:
```yaml
services:
  db:
    image: postgres:16
    environment: { POSTGRES_PASSWORD: x }
    healthcheck: { test: ["CMD", "pg_isready"], interval: 2s, retries: 10 }
    volumes: [dbdata:/var/lib/postgresql/data]
  web:
    image: nginx:alpine
    ports: ["8080:80"]
    depends_on: { db: { condition: service_healthy } }
    volumes: ["./site:/usr/share/nginx/html"]
volumes: { dbdata: {} }
```

- [ ] `cd ~/sy-test/proj && shipyard compose up -d`. Expect: the network, volume and containers made; `web` starts after `db` is healthy; exit 0.
- [ ] `curl localhost:8080`. Expect: answers (the missing `./site` directory was made).
- [ ] `shipyard compose ps`. Expect: both, `db` `(healthy)`.
- [ ] `shipyard compose logs -f web` (Ctrl-C to end). Expect: nginx's logs.
- [ ] `shipyard compose exec web sh -c 'echo in-web'`. Expect: `in-web`.
- [ ] `shipyard compose run --rm web echo once`. Expect: `once`.
- [ ] `shipyard compose top`, `stop`, `start`, `restart`. Expect: as compose.
- [ ] `shipyard ps --format '{{.Names}} {{.Label "com.docker.compose.project"}}'`. Expect: the project's labels.
- [ ] `shipyard compose up` (attached) with `--abort-on-container-exit` on a service that exits. Expect: ends with it; `--exit-code-from web` gives its code.
- [ ] Profiles: a service with `profiles: [debug]`; `shipyard compose --profile debug up -d`. Expect: started only with the profile.
- [ ] Change `web`'s image tag, `shipyard compose up -d`. Expect: recreated (compose's rename works).
- [ ] `shipyard compose down -v`. Expect: containers, network and volumes removed.
- [ ] Without the socket units (Linux) / with the VM stopped (Mac): `shipyard compose ps`. Expect: the service is started for it; it ends by itself after 60 s idle (`service.log` beside the socket).
- [ ] `DOCKER_HOST=… docker compose up -d` directly. Expect: the same.
- [ ] A service with `build: .`. Expect: doesn't work (`docker build` through the API isn't served).
- [ ] **[Linux][rootless]** A project with two services that talk by name. Expect: fails (no shared networks rootless); `network_mode: "service:db"` works; `ipc: "service:db"` works.
- [ ] **[Mac]** A project bind `./data:/data` under your home. Expect: works. One outside the shared places (`/opt/x:/x`). Expect: `mounts denied: the path /opt/x isn't shared from the Mac`.
- [ ] **[Mac]** `platform: linux/amd64` on a service. Expect: runs through Rosetta.

## 14. Docker's own CLI against the API (`DOCKER_HOST`)

Set `DOCKER_HOST` as in Setup. Everything below uses Docker's own CLI.

- [ ] `docker version`. Expect: a `Server` part saying Shipyard's engine, API 1.43.
- [ ] `docker info`. Expect: counts, kernel, CPUs, memory.
- [ ] `docker run --rm alpine echo hi`. Expect: `hi` (pulls first if missing: the create says `No such image` and the CLI pulls).
- [ ] `docker run -it --rm alpine sh`, type, resize, Ctrl-P Ctrl-Q on a non-`--rm` one. Expect: a terminal; resize follows; detach works.
- [ ] `docker run -i --rm alpine cat < big.txt | wc -l`. Expect: `200000`.
- [ ] `docker run -d --name dweb -p 8085:80 nginx:alpine; docker ps; docker ps -a --filter status=running`. Expect: listed.
- [ ] `docker logs -f --tail 5 dweb`, `docker logs --since 1m -t dweb`. Expect: as Docker's.
- [ ] `docker exec -it dweb sh`, `docker exec dweb sh -c 'exit 5'; echo $?`. Expect: a shell; `5`.
- [ ] `docker exec -d dweb sleep 30`. Expect: returns at once.
- [ ] `docker exec --privileged dweb true; echo $?`. Expect: refused (400).
- [ ] `docker inspect dweb`, `docker top dweb`, `docker stats --no-stream dweb`, `docker port dweb`. Expect: Docker's output.
- [ ] `docker stop dweb; docker start dweb; docker restart dweb; docker kill dweb; docker start dweb`. Expect: each works; a second `stop` is a quiet no-op (304).
- [ ] `docker pause dweb; docker unpause dweb`. Expect: works; pausing twice errors (409).
- [ ] `docker rename dweb dweb2; docker rename dweb2 dweb`. Expect: works.
- [ ] `docker update --memory 256m --memory-swap 512m dweb`. Expect: works.
- [ ] `docker wait <a container>`. Expect: its exit code.
- [ ] `docker attach <a -it container>`. Expect: works; detach keys work.
- [ ] `docker cp dweb:/etc/nginx/nginx.conf .` and `docker cp ./nginx.conf dweb:/tmp/`. Expect: both ways.
- [ ] `docker export dweb > d.tar`. Expect: a tar.
- [ ] `docker rm -f dweb`, `docker container prune -f`. Expect: works.
- [ ] `docker pull alpine:3.19`. Expect: Docker's JSON progress lines (`Pull complete` / `Already exists` per layer), `Digest:`.
- [ ] `docker images`, `docker image inspect alpine:3.20`, `docker tag alpine:3.20 x:1`, `docker rmi x:1`, `docker history alpine:3.20`. Expect: as Docker's.
- [ ] `docker save alpine:3.20 -o a.tar; docker load -i a.tar`. Expect: works.
- [ ] `docker import web.tar imp:1`. Expect: an ID.
- [ ] `docker push localhost:5000/app:1` (registry running). Expect: Docker's lines, then `digest:`.
- [ ] `docker login localhost:5000` / Docker Hub. Expect: `Login Succeeded` (checked by the service, kept by your CLI).
- [ ] `docker image prune -f`, `docker builder prune -f`, `docker system prune -f`, `docker system df`. Expect: work.
- [ ] `docker volume create --label tier=db dv; docker volume ls; docker volume inspect dv; docker volume rm dv; docker volume prune -f`. Expect: works; `prune` takes anonymous volumes only unless `--all`.
- [ ] `docker volume create -o type=nfs x; echo $?`. Expect: refused.
- [ ] `docker network create dn; docker network ls; docker network inspect dn; docker network connect dn <c>; docker network disconnect dn <c>; docker network rm dn; docker network prune -f`. Expect: works.
- [ ] `docker network rm bridge` / `docker network create default`. Expect: 403.
- [ ] `docker events` while you start a container. Expect: Docker's events.
- [ ] `docker events --filter daemon=x`. Expect: refused (400).
- [ ] `docker run --privileged alpine true; echo $?`. Expect: refused with what to use instead.
- [ ] `docker run --pid host alpine true`, `--shm-size 1g`, `--gpus all`, `--volumes-from x`, `--link x`. Expect: each refused with a reason.
- [ ] `docker build .`. Expect: not served.
- [ ] `docker run -v /nonexistent/dir:/x alpine ls /x` **[Linux]**. Expect: the directory is made (as Docker's daemon makes it), unlike `shipyard run -v`.
- [ ] Through the API, `curl --unix-socket "$SOCK" http://x/v1.43/_ping` and `…/v1.99/_ping`. Expect: `OK`; the second "client version … is too new".
- [ ] `curl --unix-socket "$SOCK" http://x/shipyard/links | head -c 300`. Expect: `{"version": 1, …}` with `containers`, `entries`, `links`.
- [ ] `curl --unix-socket "$SOCK" http://x/shipyard/volumes`, `/shipyard/containers/<id>/isolation`, `/shipyard/containers/<id>/sockets`, `/shipyard/networks/bridge`. Expect: JSON with `"version": 1`; isolation/sockets on a stopped one: 409.
- [ ] `curl --unix-socket "$SOCK" http://x/nope`. Expect: `404 page not found`.
- [ ] **(hostile)** **[Linux]** As another user: `curl --unix-socket /run/shipyard/api.sock http://x/_ping`. Expect: can't (root's socket is 0600 in a root-only directory). A rootless socket is yours alone.
- [ ] **[Linux]** Testcontainers: `cargo build -p shipyard-cli && cargo test -p shipyard-acceptance` (rootless, then as root). Expect: the one test passes.
- [ ] **[Mac]** Testcontainers against `docker.sock` with `TESTCONTAINERS_RYUK_DISABLED=true`. Expect: works; without it, Ryuk's bind of Docker's socket is refused (F171).

## 15. `system` commands and the API's service

- [ ] `shipyard system df`. Expect: covered in images (§8).
- [ ] `shipyard system prune` (asks). Expect: Docker's warning; `n` keeps everything.
- [ ] `shipyard system prune -f`. Expect: sections for stopped containers, unused networks, dangling images, what nothing in the store needs, build cache.
- [ ] `shipyard system prune -a --volumes -f`. Expect: also every unused image and unused anonymous volumes; named volumes kept.
- [ ] `shipyard system prune -f --filter until=1h --volumes; echo $?`. Expect: refused (`until` not with `--volumes`).
- [ ] **[Linux]** `shipyard system service` (no systemd units). Expect: listens on `api.sock` in the runtime directory; `docker ps` with `DOCKER_HOST` works; it exits after 60 s idle.
- [ ] **[Linux]** `SHIPYARD_API_IDLE=5 shipyard system service`. Expect: exits after 5 s idle.
- [ ] **[Linux]** `mkdir -m 700 /tmp/mine && shipyard system service --socket /tmp/mine/my.sock`. Expect: listens there. (In `/tmp` itself, writable by anyone, it's refused: "isn't yours alone".)
- [ ] **[Linux]** A second `shipyard system service` while one runs. Expect: refused.
- [ ] **[Linux]** Ctrl-C during a long `docker pull` through it. Expect: ends once the running request finishes; new ones get 503.
- [ ] **[Linux]** `shipyard system service --ensure; echo $?`. Expect: starts the service in the background if nothing answers, exits once it answers (0); run again: exits at once.
- [ ] **[Linux]** `shipyard system service --ensure --socket /tmp/shared/x.sock` in a directory others can write. Expect: refused.
- [ ] **[Linux]** `shipyard doctor`. Expect: says where the socket is and whether the socket unit is enabled.
- [ ] **[Mac]** `shipyard system service`. Expect: points at the VM's `docker.sock`.

## 16. Rootless specifics **[Linux][rootless]**

- [ ] `shipyard doctor` as a normal user. Expect: `user: 1000 (rootless)`; ID mapping, delegated cgroup, pasta lines `ok`.
- [ ] `shipyard run --rm alpine id`. Expect: `uid=0(root)` (you on the host).
- [ ] `shipyard run --rm -u 1000 -v $PWD:/w alpine touch /w/owned; ls -ln owned`. Expect: owned by your range's 1000th ID (e.g. 100999).
- [ ] Without `/etc/subuid` entries (another test user): `shipyard run --rm alpine true`. Expect: a warning saying what's missing; only root inside; `-u 1000` fails "user 1000 isn't mapped…".
- [ ] `shipyard run --rm --uidmap 0:100000:65536 alpine true; echo $?`. Expect: refused (root's flag).
- [ ] `shipyard run --rm --ulimit nofile=999999:999999 alpine true; echo $?`. Expect: "Operation not permitted (raising a hard limit needs root)".
- [ ] `sudo shipyard ps` vs `shipyard ps`. Expect: separate containers (root's and yours).
- [ ] From an SSH login without a user scope: `shipyard pause x`, `shipyard stats`, `-m 64m`. Expect: refused with the `systemd-run --user --scope` hint; `shipyard doctor` says so.
- [ ] Ubuntu 24.04 without the AppArmor profile: `shipyard run --rm alpine true`. Expect: refused, naming the setting and the profile; with it installed: works.
- [ ] `XDG_RUNTIME_DIR= shipyard run --rm alpine true` (e.g. after `su`). Expect: runs without a record, says so.
- [ ] Log out with a container running (`KillUserProcesses=no`). Expect: it runs on. With `yes`: `shipyard doctor` warns; use `systemd-run --user --scope shipyard run -d …` with linger.

## 17. OCI bundles and `shipyard-runtime` **[Linux]**

- [ ] Make a bundle: `mkdir -p mybundle/rootfs && shipyard export <an alpine container> | tar -x -C mybundle/rootfs`, write `mybundle/config.json` from `docs/user/bundles.md`'s example (adapted to `sh`). `shipyard run --bundle ./mybundle`. Expect: runs; no container record.
- [ ] `shipyard run --bundle ./mybundle -e A=1; echo $?`. Expect: refused (the config is the whole description).
- [ ] `shipyard-runtime create --bundle ./mybundle t1; shipyard-runtime state t1`. Expect: `"status": "created"`.
- [ ] `shipyard-runtime start t1; shipyard-runtime state t1; shipyard-runtime ps t1`. Expect: `running`; its processes.
- [ ] `shipyard-runtime pause t1; shipyard-runtime resume t1`. Expect: `paused`, then `running`.
- [ ] `shipyard-runtime exec t1 ls /`. Expect: the listing.
- [ ] `shipyard-runtime events --stats t1`. Expect: one JSON stats line.
- [ ] `shipyard-runtime kill t1 TERM; shipyard-runtime delete t1`. Expect: stopped, then gone.
- [ ] `shipyard-runtime state nosuch; echo $?`. Expect: 1. `shipyard-runtime --bogus; echo $?`: 2.
- [ ] `sudo shipyard-runtime state t1` vs yours. Expect: separate state ("no container with ID …").

## 18. The macOS VM **[Mac]**

### Start, status, stop, idle

- [ ] `shipyard vm status` with the VM stopped. Expect: stopped; the size a start takes, and from where; the guest it boots.
- [ ] `time shipyard run --rm alpine echo hello`. Expect: the VM starts (about a second once the guest is built), `hello`.
- [ ] `shipyard vm start` when it runs. Expect: returns, nothing changes.
- [ ] `shipyard vm status`. Expect: running; forwarded ports listed.
- [ ] `shipyard vm stop`. Expect: its containers stopped in order, then the VM; `vm status` says stopped.
- [ ] `shipyard vm stop` during `shipyard vm start` (two terminals). Expect: the stop succeeds once it's up; the start says "the VM is stopping".
- [ ] `SHIPYARD_VM_IDLE=60 shipyard vm start` then nothing for 60+ s; `shipyard vm status`. Expect: stopped by itself.
- [ ] Idle with `shipyard run -d alpine sleep 600` running: wait past the idle time. Expect: the VM stays up (a detached container keeps it up); `rm -f` it and it stops after the idle time.
- [ ] Idle with `shipyard logs -f` or `shipyard attach` open. Expect: stays up.
- [ ] Ctrl-C while a command waits for the VM to start. Expect: ends it before it's sent.
- [ ] `ls -l "$HOME/Library/Application Support/Shipyard/vm/"`. Expect: `host.log`, `console.log`, `data.img`, `vm.json`/`network.json`, `docker.sock` while running.
- [ ] `shipyard vm status` after a guest rebuild (`scripts/guest/build.sh`) while the VM runs. Expect: says the running VM is another build's, with what to do (`vm stop`).

### Size, clock, disk

- [ ] `shipyard vm set --memory 6g --cpus 4; shipyard vm status`. Expect: the next start takes 6 GiB / 4 CPUs.
- [ ] `shipyard vm stop; shipyard run --rm alpine sh -c 'nproc; free -m'`. Expect: 4 CPUs, about 6 GiB.
- [ ] `shipyard vm set --memory default --cpus default`. Expect: back to a quarter of the Mac's memory (4–16 GiB) and half its cores (at most 8).
- [ ] `shipyard vm set --memory 1g; echo $?`. Expect: refused (at least 2 GiB). `--memory 999g`: refused (the Mac's less 2 GiB).
- [ ] `shipyard vm set; echo $?`. Expect: a usage error (needs one of the two).
- [ ] Sleep the Mac for a few minutes with the VM running; wake; `shipyard run --rm alpine date -u; date -u`. Expect: the same time within seconds.
- [ ] `shipyard doctor | grep disk`. Expect: what the disk takes, and the Mac's free space.
- [ ] Pull a big image, `shipyard rmi` it, `shipyard image prune -a -f`, `shipyard vm stop`; compare `du -h data.img` before/after. Expect: space given back at the stop.

### Your files (shares)

- [ ] `cd ~/sy-test && shipyard run --rm -v "$PWD":/app -w /app alpine ls`. Expect: your files.
- [ ] `cd ~/sy-test && echo rel > rel.txt && shipyard run -d --name relc alpine sleep 60 && shipyard cp rel.txt relc:/tmp/ && shipyard exec relc cat /tmp/rel.txt`. Expect: `rel` (the command runs in this directory in the VM, so a relative path works). Then `shipyard rm -f relc`.
- [ ] `mkdir -p /tmp/sy && echo t > /tmp/sy/f && shipyard run --rm -v /tmp/sy:/x alpine cat /x/f`. Expect: `t`.
- [ ] `shipyard run --rm -v "$TMPDIR":/x alpine ls /x | head -2`. Expect: works (`/var/folders`).
- [ ] With a USB drive or disk image attached: `shipyard run --rm -v /Volumes/<name>:/x alpine ls /x`. Expect: works (asks for Removable Volumes permission the first time from launchd's VM).
- [ ] `shipyard run --rm -v /opt:/x alpine true; echo $?`. Expect: refused before anything runs (outside the shares).
- [ ] `ln -s /opt ~/sy-test/outlink; shipyard run --rm -v ~/sy-test/outlink:/x alpine true; echo $?`. Expect: refused (a link takes it out).
- [ ] `shipyard run --rm -v /var/lib/shipyard:/x alpine true; echo $?`. Expect: refused (the VM's own files).
- [ ] `shipyard run --rm -v "$SSH_AUTH_SOCK":/ssh alpine true; echo $?`. Expect: refused (a socket).
- [ ] `docker run --rm -v /opt/data:/x alpine true` (Docker's CLI). Expect: `mounts denied: the path /opt/data isn't shared from the Mac`.
- [ ] `docker run --rm -v /var/run/docker.sock:/var/run/docker.sock alpine true`. Expect: refused (F171).
- [ ] **(hostile)** In a container that binds `~/sy-test/share`, replace a subdirectory with a symlink to `/` *from inside the container*; restart a second container binding that subdirectory. Expect: its start is refused (a link put there makes the start fail).
- [ ] `cd ~/Desktop && shipyard run --rm -v "$PWD":/x alpine ls /x | head -2` from Terminal. Expect: works with Terminal's permission; from a launchd-started VM: refused with `macOS hasn't let the VM read /Users/you/Desktop: allow shipyard in System Settings → Privacy & Security → Files and Folders` until allowed.
- [ ] `shipyard doctor | grep -i -A2 shares`. Expect: the four shares; which guarded folders the VM can't read.
- [ ] File speed: `shipyard run --rm -v ~/sy-test:/w -w /w alpine sh -c 'time sh -c "for i in \$(seq 2000); do echo > f\$i; done; rm f*"'` vs the same in a named volume. Expect: the share 2–4× slower for many small files (known: F172).

### Terminal, input, signals, variables

- [ ] `shipyard run -it --rm alpine sh`: Tab, Ctrl-C, `vi`, resize. Expect: as on Linux.
- [ ] `shipyard run --rm alpine sh -c 'echo err >&2' > out.txt`. Expect: `err` on your terminal, not in `out.txt`.
- [ ] `echo typed-later | (shipyard ps; cat)`. Expect: `ps` doesn't eat your input; `typed-later` printed by `cat`.
- [ ] `yes | shipyard run -i --rm alpine sleep 1000`, then Ctrl-C. Expect: ends at once (signals on their own connection).
- [ ] `shipyard run --rm alpine sleep 100 & sleep 2; kill -INT %1`. Expect: the container ends.
- [ ] Close the Terminal window during `shipyard run -it alpine sh`. Expect: hung up, as on Linux.
- [ ] `shipyard run -it --name mdt alpine sh`, Ctrl-P Ctrl-Q. Expect: detaches in the VM; `shipyard attach mdt` gets it back.
- [ ] `LANG=fr_FR.UTF-8 shipyard run --rm alpine env | grep -E 'LANG|TERM'`. Expect: `LANG`, `TERM` passed.
- [ ] `export SECRET=1; shipyard run --rm alpine env | grep SECRET`. Expect: nothing. `shipyard run --rm -e SECRET alpine env | grep SECRET`: `SECRET=1`.
- [ ] A `shipyard prune` on a terminal on the Mac: `shipyard container prune`. Expect: asks, as on Linux.

### Ports on the Mac

- [ ] `shipyard run -d --name mp -p 8080:80 nginx:alpine; curl -s localhost:8080 | head -1; curl -s '[::1]:8080' | head -1`. Expect: both answer.
- [ ] From another device on your Wi-Fi: `curl http://<mac's IP>:8080`. Expect: no answer (loopback only).
- [ ] `curl --max-time 3 http://192.168.64.X:8080` (the VM's own address, from `vm status`/`host.log`). Expect: no answer.
- [ ] `shipyard run -d -p 127.0.0.1:8086:80 nginx:alpine; curl localhost:8086`. Expect: answers on `127.0.0.1` only.
- [ ] Start something on the Mac on 5432 (`nc -l 5432`), then `shipyard run -d -p 5432:5432 -e POSTGRES_PASSWORD=x postgres:16; shipyard vm status`. Expect: `not forwarded: 127.0.0.1:5432 (in use on this Mac)`; forwarded once you stop `nc`.
- [ ] Start a Mac program on every address of 8080 after `mp` is forwarded (`python3 -m http.server 8080 --bind 0.0.0.0`). Expect: `vm status`/`doctor` say `shadowed: 8080: another program on this Mac listens on every address of it…`.
- [ ] UDP: `shipyard run -d --name mu -p 5353:53/udp alpine sh -c 'nc -u -l -p 53 -e cat'`; `echo hi | nc -u -w1 127.0.0.1 5353`. Expect: `hi`; `vm status` lists `127.0.0.1:5353/udp`.
- [ ] `shipyard restart mp; curl localhost:8080`. Expect: answers within two seconds.

### launchd: `vm install`, `--at-login`

Copy `shipyard` and the guest out of the guarded folders first:
```sh
mkdir -p ~/bin && cp target/debug/shipyard ~/bin/ && scripts/macos/sign.sh ~/bin/shipyard
mkdir -p "$HOME/Library/Application Support/Shipyard/vm" && cp -R target/guest "$HOME/Library/Application Support/Shipyard/vm/guest"
```
(Remember to remove that `guest` copy after, or it shadows the tree's guest after rebuilds.)

- [ ] `~/Desktop/shipyard/target/debug/shipyard vm install; echo $?`. Expect: refused (a binary in a guarded folder).
- [ ] `~/bin/shipyard vm install`. Expect: installed; `shipyard vm stop`, then `docker ps` with `DOCKER_HOST` works at any time (it starts the VM; up to 10 s right after a stop).
- [ ] `~/bin/shipyard doctor`. Expect: no `launchd` warning.
- [ ] `launchctl` unload/disable it in System Settings › General › Login Items, then `shipyard vm status`. Expect: says installed but not loaded; `vm install` loads it again.
- [ ] `~/bin/shipyard vm install --at-login`; run `shipyard run -d --restart always --name back nginx:alpine`; `shipyard vm stop`; `shipyard vm status`. Expect: says 1 container would come back.
- [ ] Log out and in (or restart the Mac). Expect: the VM starts at login, `back` runs; with no such container, the VM stays stopped.
- [ ] `shipyard rm -f back; shipyard vm stop`; log out and in. Expect: the VM stays stopped.
- [ ] `~/bin/shipyard vm install` (without `--at-login`). Expect: the login start is off.
- [ ] `~/bin/shipyard vm uninstall`. Expect: launchd lets go of the socket; `docker ps` fails while the VM is stopped.
- [ ] Without `vm install`, after an idle stop: `docker ps`. Expect: can't connect until a `shipyard` command starts the VM; one connecting just as it stops gets a 503.

### Without the VM, and misc

- [ ] `SHIPYARD_VM=off shipyard run --no-image echo host`. Expect: runs on the Mac without isolation.
- [ ] `SHIPYARD_VM=off shipyard run alpine true; echo $?`. Expect: refused (an image needs Linux).
- [ ] Sign `shipyard` ad hoc (`codesign -f -s - target/debug/shipyard`), then `shipyard doctor | grep entitlement`. Expect: `entitlement` fails (not signed to run a VM); re-sign with `scripts/macos/sign.sh`.
- [ ] `shipyard system recover; echo $?`. Expect: not on a Mac (refused).

## 19. The desktop app

Start with a few containers so the views have something to draw:
Add a third service to `~/sy-test/proj/compose.yaml` (§13), one that talks to `web`
without declaring it:
```yaml
  caller:
    image: alpine:3.20
    command: sh -c 'while :; do wget -qO- web >/dev/null; sleep 1; done'
```
Then:
```sh
cd ~/sy-test/proj && shipyard compose up -d            # web depends_on db (service_healthy); caller undeclared
shipyard run -d --name lone -p 8090:80 nginx:alpine
shipyard run -d --name worker -m 32m alpine sh -c 'sleep 5; x=$(head -c 200m /dev/zero | tr "\0" a)'   # will be OOM-killed
```
(On Linux rootless there are no shared networks: `caller` can't reach `web` there.)

### Opening, finding `shipyard`, the VM

- [ ] **[Mac]** Open `Shipyard.app` with `shipyard` not on its paths. Expect: says none is found; Settings (⌘,) → **Choose shipyard…** fixes it.
- [ ] Settings shows the `shipyard` path in use.
- [ ] **(hostile)** Choose a `shipyard` copy in a folder others can write (e.g. `chmod o+w` a directory, put a copy there). Expect: refused, never run.
- [ ] Choose a `shipyard`, then delete that file and reopen the app. Expect: says it's gone rather than running another.
- [ ] **[Mac]** With the VM stopped and "Start Shipyard's VM when the app opens" on: open the app. Expect: shows `shipyard vm start`'s lines, then the window fills.
- [ ] **[Mac]** Turn the setting off, quit, `shipyard vm stop`, reopen. Expect: a **Start** button; clicking it starts the VM.
- [ ] **[Mac]** An ad-hoc-signed `shipyard` (`codesign -f -s - …`). Expect: the app says it isn't signed to run a VM and how (`scripts/macos/sign.sh`).
- [ ] `cat ~/Library/Application\ Support/Shipyard/desktop.json; ls -l ~/Library/Application\ Support/Shipyard/desktop.json`. Expect: the settings, readable by you alone (0600).
- [ ] Edit `desktop.json`'s version to a higher number, reopen. Expect: the app uses defaults, says so, and never writes over it.
- [ ] Quit the app. Expect: nothing else stops (containers and VM keep running).

### The side rail, title bar and status bar

- [ ] The rail: **Local** sections Yard, Images, Volumes, Networks, each with a count. Expect: counts match `shipyard ps -a`/`images`/`volume ls`; Networks counts the bridge networks alone (`network ls` also lists `host` and `none`).
- [ ] The rail's host block. Expect: the VM's kind, memory with a bar, kernel.
- [ ] The title bar: **Run an image…** with `⌘K`. Expect: opens the Run sheet.
- [ ] The status bar. Expect: `Daemonless — nothing runs when no containers do`, `N monitors · X MiB`, the backend (rootless or root, cgroup v2, overlayfs) and the version.
- [ ] `shipyard rm -f lone` from a terminal. Expect: the window updates within a moment (events), counts too.
- [ ] **[Mac]** `shipyard vm stop` from a terminal while the app is open. Expect: the window says the VM is stopped and offers **Start** (it never starts it again by itself).

### The Map

- [ ] Switch the view to **Map** (Yard / Map / Manifest switch). Expect: containers left to right by dependency, starting from the published ports (host port chips at the left).
- [ ] The `web → db` edge. Expect: a **solid** line (declared `depends_on`).
- [ ] The `caller → web` edge. Expect: a **dotted orange** line (undeclared, observed), and the status line says `1 undeclared connection detected.`
- [ ] Dots move along lines with open connections, faster with more. Expect: moving dots; with System Settings → Accessibility → Display → Reduce motion on: none.
- [ ] `shipyard stop db`. Expect: within a few seconds `web → db` turns **red dashed** (refused); `db` stopped; `web` outlined **degraded**, anything further up **affected**; the line above the map names them and offers **Start db** (tooltip `shipyard start db`).
- [ ] Click **Start db**. Expect: `db` starts; the line goes back to healthy.
- [ ] Box sizes: `worker` (`-m 32m`) a short box; a container without a limit the long 40′ box. Expect: as described.
- [ ] Each box's lamp shows its state; a coloured stripe per network above it; named-volume chips below.
- [ ] Hover a container. Expect: what it needs and what needs it traced, the rest fades.
- [ ] Hover a network stripe or a volume chip. Expect: who's on it lit.
- [ ] Click a container. Expect: selected; the inspector shows it.
- [ ] **Show** toggles: **Networks**, **Volumes**, **Images**. Expect: network keys appear (click one: opens Networks on it); volume chips; each container's image as a strip of layers above its box.
- [ ] Below the map: each compose project's start order (`db` before `web`), and containers connected to nothing.
- [ ] With no links at all (`shipyard compose down`, remove `caller`, keep one plain container): Expect: `No links between containers yet.` and `A container that publishes a port, declares depends_on or connects to another is drawn here.`
- [ ] With links all healthy. Expect: `All N active links healthy.` / `The one active link is healthy.` and `Every connection is declared.` when none is undeclared.
- [ ] Remove the `db` service's container (`shipyard rm -f proj-db-1`). Expect: a refused edge to a missing service (`proj/db`).
- [ ] **[Linux][rootless]** Expect: the map draws declared links only, and says undeclared connections can't be seen here (hover for why).

### The Yard

- [ ] View **Yard**. Expect: each compose project as a bay of containers, standalone ones in a bay of their own, with the networks each project uses (net keys).
- [ ] Select a container. Expect: the yellow crane hangs over it.
- [ ] Click a net key. Expect: opens Networks on that network.
- [ ] The tally above (`N running`, `N degraded`, `N exited`…). Expect: matches.

### The Manifest

- [ ] View **Manifest**. Expect: each container's ID, name, image, state (and exit code), memory limit, published ports and uptime.
- [ ] `shipyard run -d --name newone alpine sleep 600` in a terminal. Expect: appears without a refresh.
- [ ] Leave it open 30+ s. Expect: stays current (reads again every 30 s).

### The inspector

- [ ] Select `web`. Expect: its code (serial), state and image; **Dependencies**: what it needs (`db`) and what needs it (`caller`), as trees; an impact sentence for stopping it.
- [ ] Select `caller` (its connection to `web` isn't declared). Expect: the `depends_on` lines to merge under the file's `services:` and the compose file they go in; **Copy** copies them (paste to check). The app never edits the file.
- [ ] A standalone container that connects to `web` (`shipyard run -d --name outsider --network proj_default alpine sh -c 'while :; do wget -qO- web >/dev/null; sleep 1; done'`). Expect: an undeclared edge on the map, but no `depends_on` lines in its inspector (compose can't declare it).
- [ ] **Attached**: image (layers, size), networks and addresses, volumes and binds. Expect: clicking the image, a network or a volume opens that section on it.
- [ ] **Resources**: `live · 1s`, CPU and memory against `memory.max`, the last 48 s drawn. Expect: moves with `shipyard exec web sh -c 'while :; do :; done'`.
- [ ] Stop the container while selected. Expect: `not live`, its reason on hover.
- [ ] Hide the window (⌘H) a minute, show it. Expect: followed again.
- [ ] **Isolation** (running): its namespaces (own or the host's), cgroup path, PID 1 and the command with host PIDs, the user map (rootless). Expect: matches `curl …/shipyard/containers/<id>/isolation`.
- [ ] Published ports and uptime. Expect: correct.
- [ ] Select `worker` after it's OOM-killed. Expect: an alert with the limit it hit (out of memory).
- [ ] **(hostile)** `shipyard run -d -l 'com.docker.compose.project=<img src=x onerror=alert(1)>' -l 'com.docker.compose.service=<b>svc</b>' --name hostile alpine sleep 600`. Expect: the Yard bay and inspector show the text literally; no image, no alert, no bold.

### Actions

- [ ] Each button's tooltip is its CLI twin (`shipyard stop web`, `shipyard pause web`, …). Expect: yes.
- [ ] **Stop**, **Start**, **Pause**, **Resume**, **Restart** on `lone`. Expect: each acts; the views update.
- [ ] **Restart** on a container you stopped. Expect: greyed out (start it instead).
- [ ] **Remove** on a running container. Expect: asks in place, saying what it does and its command (`shipyard rm -f …`); confirm removes it.
- [ ] **Remove** on `db` while `web` (running) needs it. Expect: greyed out, tooltip names `web` (stop them first).
- [ ] **Shell**. Expect: opens the console's Shell tab.

### The Run sheet

- [ ] ⌘K (or **Run image**, or the title bar). Expect: a sheet with image, name, memory, port.
- [ ] Type `alpine:3.20`, memory `256m`. Expect: the echo reads `shipyard run -d --memory 256m alpine:3.20`.
- [ ] Add name `sheet1`, port `8081:80`, image `nginx:alpine`. Expect: `shipyard run -d --name sheet1 --memory 256m -p 8081:80 nginx:alpine` (or the same flags in its order); run it: selected in the Yard; `curl localhost:8081` answers.
- [ ] Port `80` only. Expect: a free host port.
- [ ] An image Shipyard doesn't have (`busybox:1.36` after `shipyard rmi`). Expect: pulled with its progress shown, then run. (A big image's progress may stand still for minutes: F192.)
- [ ] `alpine` with no command. Expect: runs and shows as exited (its shell exits at once).
- [ ] A bad memory value (`lots`). Expect: refused before anything runs.
- [ ] Escape. Expect: the sheet closes.

### The console

- [ ] Select `web`. Expect: the console under the view: its name, image, host PID; **Logs** and **Shell** tabs.
- [ ] **Logs**. Expect: the last 500 lines, then new ones as they come (curl it); colours stripped (`shipyard run -d --name col alpine sh -c 'while :; do printf "\033[31mred\033[0m\n"; sleep 1; done'` shows `red` plain).
- [ ] **Shell**. Expect: a real terminal (`sh` in it): prompt, Tab completion, Ctrl-C ends `sleep 100`, `top` and `vi` full-screen.
- [ ] Resize the dock (drag) / the window. Expect: the terminal follows (`stty size` changes).
- [ ] Ctrl-P Ctrl-Q in the shell. Expect: typed like any keys (no detach).
- [ ] `yes` in the shell. Expect: a flood shown as fast as it draws; Ctrl-C stops it; the window stays responsive.
- [ ] `exit`. Expect: the shell ends; Enter opens another.
- [ ] Switch to Logs and back, fold with the chevron and unfold. Expect: the shell stays open.
- [ ] Select another container. Expect: the shell closes.
- [ ] Stop the container. Expect: the shell closes.
- [ ] An image without `sh` (e.g. a distroless one). Expect: the Shell can't start (it runs `shipyard exec -it NAME sh`).
- [ ] **[Mac]** On macOS 13.0–13.2. Expect: the Shell tab says it needs 13.3.

### Images

- [ ] Open **Images**. Expect: each image a strip of its layers, bottom to top, widths by size; images on the same base are one family, the shared base coloured alike (named `alpine:3.20 base` when such an image is stored); an image's own layers faint.
- [ ] The line at the top. Expect: `N images`, what's on disk (each layer once), and what sharing saves; `N unused`.
- [ ] Hover a layer. Expect: it lights up wherever it's stored; the caption says the step that made it, its size, which images share it; **no flicker** as you move along it.
- [ ] Select an image. Expect: its layers top to bottom and who uses it (click a user: goes to that container).
- [ ] **Run** on an image. Expect: the Run sheet opens with it; an image pulled by digest alone has no name to run it by.
- [ ] **Remove** on an image a container uses. Expect: greyed out.
- [ ] **Remove** on an unused image, confirm. Expect: as `shipyard image rm` of its tags; its own layers freed, shared layers stay.
- [ ] **Remove N unused**, confirm. Expect: those images removed; any a container started using meanwhile refused, not forced.
- [ ] Map's **Images** toggle. Expect: each container's image as a strip above its box.

### Volumes

- [ ] Open **Volumes**. Expect: each volume a drum filled by its size against the largest; dashed when no container mounts it; who mounts it (where, read-only or not).
- [ ] A huge or deep volume (`shipyard run --rm -v big:/b alpine sh -c 'mkdir -p /b/d && cd /b/d && seq 1 250000 | xargs touch'`). Expect: "at least" before its size.
- [ ] Select a volume. Expect: who mounts it and who can write, where its data is (in the VM, on a Mac), its top-level files.
- [ ] **Delete** on a volume any container names (even stopped). Expect: greyed out.
- [ ] **Delete** on an unused volume, confirm. Expect: as `shipyard volume rm`; gone.
- [ ] **Delete N unused**, confirm. Expect: those deleted; one a container names since is refused, never forced.
- [ ] The map's volume chips and the inspector's volumes. Expect: open the volume here.

### Networks

- [ ] Open **Networks**. Expect: each bridge network a bus: subnet, bridge, gateway, each container with its address (and the other network it's on, or the host port it publishes as tags); `NAT → internet`, or `⊘ no route out` for an internal one.
- [ ] Make `shipyard network create --internal intra2` and a container on it. Expect: `⊘ no route out`; its inspector: "Internal networks have no NAT rule, so containers on intra2 can reach each other but not the internet."
- [ ] Select a network. Expect: each container's address and the host's end of its veth pair; the containers joining it to another network; a **DNS** line (whether Shipyard's DNS server runs on it; none on `bridge`).
- [ ] The map's and Yard's net keys and the inspector's networks. Expect: open it here.
- [ ] **[Linux][rootless]** Expect: no bridge networks, and says so.

### Settings, appearance, window life

- [ ] ⌘, opens Settings in the inspector's place: **Start Shipyard's VM when the app opens** (Mac), the `shipyard` path, **Choose shipyard…**.
- [ ] System Settings → Appearance → Light, then Dark. Expect: the window follows; both readable.
- [ ] Hide/minimise the window for 5+ minutes with nothing running. Expect: the app lets go of its connections; the VM's idle stop comes (`shipyard vm status`).
- [ ] Show it again. Expect: reconnects (starting the VM if the setting says so).
- [ ] `shipyard vm stop` from a terminal with the window open. Expect: says it's stopped; doesn't start it again by itself; **Start** does.
- [ ] `shipyard vm start` from a terminal. Expect: the window reconnects and loads everything again.
- [ ] With the VM in a loop of failing starts (e.g. rename the guest dir). Expect: waits longer before each new try (up to 30 s); put it back after.
- [ ] **[Mac]** A VM run from another directory (`SHIPYARD_VM_DIR=~/othervm shipyard vm start`). Expect: not seen by the app (known: F189).
- [ ] **(hostile)** The window loads nothing from the network: turn Wi-Fi off and reopen. Expect: fonts and page as before.

### The app on Linux **[Linux]**

- [ ] Build and run on Ubuntu 24.04+/Fedora 40+ (WebKitGTK 4.1): `cd desktop/ui && npm ci && SHIPYARD_BIN=/usr/local/bin/shipyard npm run tauri dev`. Expect: the window opens.
- [ ] With no service on your socket (socket unit disabled, nothing running). Expect: the app starts it (`shipyard system service --ensure`); it ends by itself a minute after the app stops using it.
- [ ] The rail. Expect: **Shows: your own**; Settings says why (root's containers aren't shown).
- [ ] `sudo shipyard run -d --name rootone alpine sleep 600`. Expect: not shown.
- [ ] The map says undeclared links can't be seen; Networks says rootless has no bridge networks.
- [ ] `mkdir -m 700 /tmp/mine; SHIPYARD_API_SOCKET=/tmp/mine/x.sock shipyard-desktop`. Expect: the app uses that socket (and starts a service there if nothing answers).

Cleanup after the app section: `cd ~/sy-test/proj && shipyard compose down -v; shipyard rm -f $(shipyard ps -aq); shipyard volume prune -f; shipyard network prune -f`.

---

## Known limitations (don't file these)

From the docs' own limitation lists (`docs/user/README.md`, `desktop.md`, `macos.md`, `images.md`, `compose.md`, `volumes.md`, `events.md`, the CHANGELOG's known limitations) and the open findings they name.

**General (README)**

- `docker build` through the API isn't served (`shipyard build` is how to build); a push sends one platform's image (no multi-platform index), each blob uploaded whole.
- Rootless containers have no shared networks: each gets its own (pasta); bridge networks and names between containers are root's.
- Other firewalls are left to you: firewalld, ufw or Docker's rules can keep root's containers from the outside; Shipyard warns with the fix but never changes them.
- No default memory limit (as Docker); processes are limited to 2,048 by default where `pids` reaches.
- The default network `bridge` reaches the host and the cloud's metadata; it can't have `--isolate-host`.
- Restart policies have edges: the boot unit doesn't know stores set with environment variables until given them; on a full disk an end is recorded (and its policy applied) once there's room.
- Not every Docker flag yet. `inspect` leaves out `ShmSize`, the image's manifest descriptor and some resource fields; its `/etc` paths are empty once a container has ended.
- Not a hardened security boundary yet.

**Images and build**

- No signature verification; no cross-repository blob mount; an upload isn't resumed (a broken one starts over).
- No untagged images (a `build`, `load` or `import` without a name is refused).
- No build context from standard input or a URL, no `--ssh`, `--cache-from`/`--cache-to`; no platform that doesn't run here (no qemu).
- zstd layers refused; no `fuse-overlayfs` fallback.
- `ps`'s `Ports` lists each port on its own (Docker groups a range); `--format json` writes `<`, `>`, `&` as they are.

**Containers**

- `--privileged` and `--init` are refused. `update` can't lift a limit (`--pids-limit -1`) or set device rates. `exec --detach-keys` refused.
- Without a terminal, Ctrl-C during `attach` ends the attachment and leaves the container running (this version).
- `attach`'s `logs=1` through the API isn't supported (400).
- Events: no `exec_create`/`exec_start`/`attach` events; none from `shipyard-runtime`.
- Volumes: no volume drivers or options.

**Compose**

- No `build:` (no `docker build` through the API).
- Rootless: no project network; services can't reach each other by name.

**On a Mac**

- No installer yet: the guest is built from the source tree, `shipyard` signed by `scripts/macos/sign.sh` (F178).
- The VM keeps the memory it was given (F168).
- Small files on a share are 2–4× slower than OrbStack's (F172).
- A bind of Docker's socket is refused; Testcontainers' Ryuk needs `TESTCONTAINERS_RYUK_DISABLED=true` (F171).
- A build context must be in your home (not `/tmp`, `$TMPDIR`, `/Volumes`) (F181).
- Without `vm install`, Docker's socket is there only while the VM runs.
- After the Mac restarts, restart-policy containers come back at the next `shipyard` command, or at login with `vm install --at-login`.
- A second container publishing the same port number on another of the VM's addresses isn't forwarded.
- `shipyard` itself has no detach keys: a third SIGINT, or a SIGTERM not handled within 10 s, makes it let go.
- File watching: a change on the Mac isn't seen as it happens by a program in a container (it has to poll).
- A repaired data disk's `lost+found` isn't reachable from the Mac.
- Upgrading from 0.4 takes a `vm stop`, a guest built again and `vm install` again.

**The desktop app**

- Not packaged yet (run from the source tree; `Shipyard.app` not notarized: it opens on the Mac that built it).
- A VM run from another directory (`SHIPYARD_VM_DIR`) isn't seen by the app (F189).
- The Run sheet's pull progress stands still on a big layer (F192).
- `/shipyard/volumes` walks every volume against one 3 s deadline: a big volume early in the list can leave later ones "at least" 0 (F194, ROADMAP).
- Deleting a volume of millions of files can outlast the app's action wait (F195, ROADMAP).
- The Shell needs macOS 13.3 or later.
