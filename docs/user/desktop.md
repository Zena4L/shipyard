# The desktop app

Shipyard's desktop app shows your containers in a window; this page says what it
does. It runs on a Mac
(Apple Silicon, macOS 13 or later) and on Linux, built there from source (it needs
WebKitGTK 4.1: Debian 13, Ubuntu 24.04 and Fedora 40 have it; it's tested on Debian 13).

It isn't packaged yet: you run it from the source tree (below).

## What it does

- **It finds Shipyard.** The app runs the `shipyard` program, chosen in this order:
  the one you picked in Settings, `SHIPYARD_BIN`, one beside the app, then
  `/opt/homebrew/bin/shipyard` and `/usr/local/bin/shipyard`, then your `PATH`.
  An app opened from the Finder doesn't see your shell's `PATH`, so pick it in
  Settings if none is found. It runs only a `shipyard` that you or root own,
  in folders no one else can write to: one another user could replace is never
  run. If the one you picked is gone or can't be run, the app says so rather
  than running another.
- **It starts Shipyard's VM when it opens** (`shipyard vm start`), showing the
  start's lines, unless you turn that off in Settings. Then a **Start** button
  starts it. The app checks first that `shipyard` is signed to run a VM, and says
  how to sign it if it isn't (`scripts/macos/sign.sh`).
- **On Linux it shows your own containers,** those of your own (rootless) Shipyard.
  If nothing answers on your API socket when it opens, it starts Shipyard's service
  for you (`shipyard system service --ensure`, as `shipyard compose` does); the
  service ends by itself once nothing has used it for a minute. Root's containers
  (`sudo shipyard …`) aren't shown: the rail says "Shows: your own", and Settings
  says why. Rootless, the map can't see the connections between containers (each
  has a network of its own, through pasta), and the Networks section has no bridge
  networks: both say so. The service the app starts runs with the app's session (its
  resource limits, its `PATH` for credential helpers) until it ends, idle: a container
  made through it meanwhile, from the app or a terminal, gets that session's limits.
- **The Map** (the view it opens on) draws your containers left to right by
  what depends on what, starting from the ports they publish on your Mac (or
  Linux host):
  - a **solid** line is a dependency compose declares (`depends_on`);
  - a **dotted yellow** line is traffic seen between two containers that no
    `depends_on` declares ("undeclared");
  - a **red dashed** line is a declared dependency that's refused: the
    container it needs isn't running, or isn't healthy when compose waits for
    that, or the service has no container;
  - **dots** move along a line while it has open connections, faster with more
    (none if your Mac asks for reduced motion). The map reads them every 2
    seconds while it's shown.

  A box's length says its memory limit (the long 40′ box: over 512 MiB, or
  none), its lamp its state, a coloured stripe above it each network it's on,
  and the chips below it its named volumes. A container that runs while
  something it needs is refused is outlined as **degraded**; one further up its
  chain as **affected**. The line above the map says what's wrong, and offers
  **Start** for the stopped container (as `shipyard start NAME` would). Hover a
  container to trace what it needs and what needs it (the rest fades); hover a
  network or a volume to see who's on it; click a container to select it. Below
  the map: each compose project's start order, from its declared dependencies,
  and the containers connected to nothing. Rootless, traffic between containers
  can't be seen (each has a network of its own), so the map draws declared
  dependencies alone and says so.
- **The Yard** shows each compose project as a bay of containers (standalone ones in a
  bay of their own), with the networks each project uses; the blue crane hangs over the
  container you select.
- **The inspector** (on the right) says everything about the container you select: its
  code, state and image (and, after an out-of-memory kill, the limit it hit); what it
  needs and what needs it, as trees, with what stopping it would do; for a connection it
  makes that its compose file doesn't declare, the `depends_on` lines to merge under the
  file's `services:` and the file they go in, to copy (the app never edits your file; a
  connection to a container of another project, or of none, gets no lines: compose can't
  declare it); its image (its layers and size),
  networks and addresses, volumes and binds; its live CPU and memory against its
  `memory.max`, the last 48 seconds drawn ("not live" if its stream ended, the reason on
  hover; followed again after the window was hidden or the connection came back); its isolation as the kernel has it (its own
  namespaces, its cgroup, its command and host PID, its user mapping), while it runs; its
  published ports and uptime.
- **Its buttons act on it:** Stop or Start, Pause or Resume, Restart and Remove. Each
  button's tooltip is the command that does the same (`shipyard stop api`). Remove asks
  first, in the inspector itself, saying what it does and its command; on a running
  container it stops it first (`shipyard rm -f`). It's greyed out while running
  containers need the one selected (its tooltip names them: stop them first), and Restart
  for a container you stopped (start it instead). Shell opens a terminal in it (the console, below).
- **The console** sits under the Yard, the Map and the Manifest while a container is
  selected: its name, image and host PID, and two tabs. **Logs** shows its last 500
  lines, then each new one as it comes (as `shipyard logs -f NAME`), as plain text: colours
  and other escape sequences are taken out. **Shell** opens a terminal in it (as `shipyard
  exec -it NAME sh`, so the image needs a `sh`): prompts, Tab, Ctrl-C and full-screen
  programs (`top`, `vi`) work, and it follows the dock's size. The shell stays open behind
  Logs or a folded console; it closes when you select another container, the container
  stops, or the window has been hidden for five minutes. `exit` ends it; Enter opens
  another. Ctrl-P Ctrl-Q is typed into the shell like any keys (it doesn't detach, as `docker
  exec` would). A flood of output is shown as fast as the window can draw it: the
  container's terminal waits for the window, as a real terminal makes a program wait. The
  inspector's **Shell** button opens this tab. The chevron folds the console to one bar.
  The shell needs macOS 13.3 or later; on 13.0–13.2 its tab says so.
- **Images** shows every image as a strip of its layers, bottom to top, as wide as their
  size. Images that start from the same base are one family, its shared base coloured
  alike (named after the stored image that is that base, `alpine:3.20 base`, when there is
  one); an image's own layers are faint. The line at the top says what's on disk, each layer
  counted once, and what sharing saves. Hover a layer: it lights up wherever it's stored, and
  the caption says the step that made it, its size and which images share it. Select an
  image for its layers top to bottom and who uses it; **Run** opens the Run sheet with it
  (an image pulled by digest alone has no name to run it by), **Remove** (as `shipyard image
  rm` its tags, confirmed in place, greyed out while any container uses it) frees its own
  layers, and **Remove N unused** removes the images it lists, those no container uses, the
  same way: shared layers stay. Neither ever forces: Shipyard refuses an image a container
  uses, even one the window didn't know about yet. Sizes are the layers' stored (compressed)
  sizes. On the map, **Images** shows each
  container's image as a strip above its box.
- **Volumes** shows every volume as a drum, filled by its size against the largest, dashed
  when no container mounts it, with the containers that mount it (where, read-only or not).
  "At least" before a size means its walk stopped short (a huge or deep volume, a mount
  inside it). **Delete N unused** deletes the volumes it lists, those no container names,
  running or not, after you confirm. Select a volume for who mounts it and who can write,
  where its data is (in the VM, on a Mac) and its top-level files; **Delete** (as `shipyard
  volume rm`, confirmed in place) is greyed out while any container names it. Neither ever
  forces: Shipyard refuses a volume a container names, even one made since. The map's volume
  chips and the inspector's volumes open it there.
- **Networks** shows each bridge network as a bus: its subnet, its bridge, the gateway,
  each container on it with its address (and the other network it's on, or the host port it
  publishes), and whether it has a way out (an internal one has none). Select one for each
  container's address and the host's end of its veth pair, the containers that join it to
  another network, and whether Shipyard's DNS server runs on it. Rootless Linux has no bridge
  networks (each container gets its own through pasta), and says so. The map's and Yard's
  network keys and the inspector's networks open it there.
- **Run image** (or ⌘K, or the title bar's **Run an image…**) opens a sheet: an image,
  a name (or none: Shipyard picks one), a memory limit and a port to publish
  (`8081:80`, or `80`). It says the command it stands for as you type
  (`shipyard run -d --memory 256m alpine:3.20`), pulls the image if Shipyard doesn't have
  it (its progress shown), runs it detached, and selects it in the Yard. It runs the
  image's own command: one that exits at once (`alpine`'s shell) shows as exited. Escape
  closes it.
- **It shows your containers** in the **Manifest** view: each container's ID, name,
  image, state (and exit code), CPU and memory in use (against its limit), published
  ports and uptime. CPU and memory are read every 2 seconds while the window is seen:
  CPU as `docker stats` says it (100 % is one CPU busy), shown from the second read;
  memory less what the kernel can drop at once. A container with no cgroup of its own
  (rootless without a delegated cgroup) shows "—". The list
  stays current as containers start, stop and go: the app follows Shipyard's
  events, reads the list again every 30 seconds, and loads everything again
  after each reconnect. If a background read fails, what's shown stays and the
  status bar says it's not current, until a read works again.
- **The side rail** counts your images, volumes and networks and says what the VM
  runs (its kernel, and its memory in use of all it has: on Linux, the host's;
  in use is what the kernel can't give back without swapping). **The status bar**
  says how many monitors run and the memory they use (each one's cgroup), and how
  Shipyard runs (rootless or root, cgroup v2, overlayfs, its version).
- **Light or dark:** the app follows your system's appearance until you press the sun/moon
  button in the title bar; it then keeps your choice from then on (it no longer
  follows the system).
- **Settings** (⌘,): start the VM when the app opens, and which `shipyard` to run.
  They're kept in `~/Library/Application Support/Shipyard/desktop.json`, readable
  by you alone. A settings file from a newer app is left as it is (the app uses
  its defaults and says so).

## Daemonless, as Shipyard is

The app is a client of Shipyard's API (the same `docker.sock` Docker's clients
use), nothing more. Quitting it stops nothing but itself. While its window is
hidden or minimised for 5 minutes, it lets go of its connections, so the VM's idle
stop can come; shown again, it reconnects (starting the VM if the setting says
so). If you stop the VM yourself (`shipyard vm stop`), the app says it's stopped
and doesn't start it again on its own: the Start button does. If its connection
keeps ending at once, it waits longer before each new try (up to 30 seconds).

## Safety

Container names, labels and images are shown as text, never as markup. The window
loads nothing from the network (its fonts are bundled), never leaves the app's own
pages, and its page may ask the app for its few commands alone (nothing touches your
files or runs a program from the page).

## On Linux

The app finds your own (rootless) API socket, `$XDG_RUNTIME_DIR/shipyard/api.sock`
(`SHIPYARD_API_SOCKET` for another), and starts the service there if nothing answers
(`shipyard system service --ensure`; with systemd's socket unit enabled, `systemctl
--user enable --now shipyard.socket`, the socket answers already: see
[the Docker API](api.md)). Run as root, it shows root's containers instead.

## Not yet

The app uses
the VM in its default place (`~/Library/Application Support/Shipyard/vm`): a VM
you run from another directory (`SHIPYARD_VM_DIR`) isn't seen by it yet.

## Running it from the source tree

You need Node.js 26 and Rust (the repository's toolchain); on Linux, WebKitGTK
4.1's development files too (Ubuntu: `libwebkit2gtk-4.1-dev libssl-dev
libayatana-appindicator3-dev librsvg2-dev libxdo-dev`).

```sh
scripts/macos/sign.sh target/debug/shipyard      # on a Mac: once after each build of shipyard
cd desktop/ui
npm ci
SHIPYARD_BIN="$PWD/../../target/debug/shipyard" npm run tauri dev
```

On a Mac, `npm run tauri -- build --bundles app` makes `Shipyard.app`
(`desktop/target/release/bundle/macos/`); `scripts/macos/sign.sh --bundle` signs it
with your local identity (`scripts/macos/make-signing-identity.sh`), so macOS asks
for its permissions once. It isn't notarized: it opens on the Mac that built it.

`docs/DEVELOPMENT.md` has the app's checks.
