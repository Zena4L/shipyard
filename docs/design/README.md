# UI design

[`shipyard-ui.html`](shipyard-ui.html) is the interactive concept for Shipyard's desktop UI
(roadmap stage 14). It's a single self-contained file with all state mocked in-page. Open it
in any browser; no build step needed.

- **Live version:** https://claude.ai/artifact/HzWgUs7ewr5ANtKKC3p7QP (private to the owner)
- **Source of truth:** the file in this folder. Edit it here, then republish.

The mock is a design reference, not production code. When the real UI is built, carry over
the concepts, layout and interaction rules below, not the code.

## The idea

The product is called Shipyard, so the UI takes the metaphor literally, but only where it
carries real information:

| Visual | What it encodes |
|---|---|
| Containers drawn as shipping containers | 20′ box = `memory.max` ≤ 512 MiB, 40′ box = > 512 MiB |
| ISO 6346 container IDs (`SHPU 304812 2`) | Real freight-container format; the boxed digit is a real check digit |
| Cobalt gantry crane over the yard | Follows the current selection |
| Bays (rows) in the yard | Compose stacks |
| Drums | Volumes; fill = size relative to the largest |
| Colored layer strips | Image layers; shared base layers share a color |
| Network bus diagrams | Bridge networks: gateway, veth taps with IPs, NAT or no egress |

## Screens

**Yard** (section), with three views of the same containers:

- **Yard:** the spatial overview with the crane and bays.
- **Map** (default): dependency graph laid out left→right by dependency depth, starting
  from published host ports.
  - Solid edge = declared (`depends_on`).
  - Dotted orange = *observed but undeclared* (seen in the container's `/proc/net/tcp`).
  - Red dashed = refused (target down).
  - Moving dots = live traffic.
  - Overlays (toggle): network color stripes on top of each node, volume chips below, image
    layer strip above.
  - Footer: start order (from declared links only) and unconnected containers.
- **Manifest:** a plain table for scanning.

**Images:** each image is a strip of layers, bottom → top. Hover a layer to highlight it in
every image that shares it. Summary shows disk used vs. saved by sharing.

**Volumes:** drum per volume, its mounts (container, path, ro/rw), a file listing, and
unmounted volumes flagged.

**Networks:** one bus diagram per network. Containers on more than one network are tagged
("also frontend"), because they are the only path between the networks.

**Inspector** (right pane), always showing whatever is selected:

- **Container:**
  - ID, state, and actions.
  - Dependencies as *Needs* / *Needed by* trees, with an impact sentence ("If postgres
    stops…") and a one-click fix for undeclared dependencies.
  - Attached image, networks and volumes.
  - Live CPU and memory against `memory.max`.
  - Isolation: namespaces on/off, cgroup path, PID 1 vs. host PID.
- **Image / volume / network:** details, usage, and safe actions.

**Console** (bottom of the Yard section): Logs and Shell for the selected container,
docked under the yard and filling whatever height the yard doesn't use. Its header shows the
container, image and host PID. Selecting another container switches it; it can be collapsed
to a single bar. On wide screens the window has a fixed app height, so the inspector scrolls
on its own instead of stretching the page.

**Status bar:** "Daemonless": the number of per-container monitor processes and their
memory, plus the backend (rootless, cgroup v2, overlayfs).

## Interaction rules (keep these)

1. **Hover traces, click navigates.**
   - Hovering a container dims everything not in its dependency chain.
   - Hovering a network, volume or image dims everything not attached to it.
2. **Everything cross-links.** Any image, network, volume or container mentioned anywhere is
   clickable and jumps to its page with it selected.
3. **State is shown, not explained.** Health appears as degraded or affected outlines, refused
   edges, and lamps. Text appears only where a decision is needed.
4. **Explain the consequence, then offer the fix.** Examples: "api is degraded because
   postgres is stopped" plus a **Start postgres** button; "worker uses postgres but doesn't
   declare it" plus **Add to depends_on**.
5. **No modal dialogs.** Destructive actions confirm inline, and are disabled (with the
   reason as a tooltip) while something depends on them.
6. **Show the kernel.** Namespaces, cgroup paths, `memory.max`, veth pairs, overlayfs
   layers and NAT rules are surfaced as UI. They are what Shipyard is built on.
7. **Every action has a CLI twin.** The run form shows the equivalent `shipyard run …`
   command as you type.

## Visual tokens

The palette is **Cobalt Yard** (chosen 2026-10-28): navy containers on blue-tinted greys,
one cobalt signal. Blue is Shipyard acting for you: what you selected, the primary
action, a restart under way. Status colours are their own set (green, yellow, red,
slate-blue, each tuned for light and dark), never the brand's.

| Token | Light | Dark | Use |
|---|---|---|---|
| `--ground` | `#e3e8f0` | `#05080f` | page background |
| `--window` / `--panel` | `#ffffff` / `#f4f6fa` | `#0f1526` / `#0b101e` | app surface / rail, title and status bars |
| `--ink` / `--muted` | `#0b1530` / `#4e5b76` | `#e6ebf5` / `#8994ab` | text |
| `--steel` | `#182b57` | `#24396e` | container bodies, charts |
| `--link` | steel and ink mixed | the same mix | a link between boxes |
| `--accent` | `#2f54ff` | `#5b7bff` | cobalt: the crane, the selection and the primary action only |
| `--ok` / `--warn` / `--bad` / `--pause` | `#16a34a` / `#c29400` / `#d92d3a` / `#6b7a99` | `#2bd47a` / `#f5c518` / `#f25563` / `#8f9bb8` | running / degraded and undeclared / failed and refused / paused |
| `--warn-ink` | `#8a6a00` | `#f5c518` | the yellow as text (the light theme's fill is too faint to read) |
| `--net-a` / `--net-b` | `#0e9f9f` / `#8b5cf6` | `#2dd4bf` / `#a78bfa` | network colors (teal, violet: never blue) |
| `--lay-*` | alpine `#0ea5e9`, bookworm `#db2777`, distroless `#65a30d` | lighter | base-layer families |

**Motion.** One curve (`--ease`) and three speeds (`--t-fast` 120 ms, `--t-base`
180 ms, `--t-slow` 420 ms for the crane and bars). What moves in a loop or on a view's
entrance is transform, opacity or a motion path; colour and width changes are short
transitions on a state change. A view or section comes in with a short fade and a 4 px
rise; the CPU graph slides one step with each second's sample; the map's dots are CSS
motion paths along their links (no frame loop); all of it stops under reduced motion.

**Theme.** The title bar's sun/moon button ("Dark mode", a toggle) flips between light
and dark over the system's choice (`data-theme` on the root, which every token block
honours), and is remembered for good; until it's first pressed, the system's setting
rules.

Type: **Big Shoulders Stencil** (IDs and headings only, like container stencils),
**Hanken Grotesk** (UI), **IBM Plex Mono** (paths, IPs, logs).

## Data the UI assumes

This is useful when designing the engine's API. The UI needs these per container:

- identity: `name`, `stack`, `image`, ISO serial
- lifecycle: `state` (running / paused / exited / failed / restarting), exit code
- limits and usage: `memMax`, live `mem`, `cpu`
- ports: published host ports
- process: `pid1`, host PID
- isolation: namespaces in use
- attachments: `nets` (network → IP), `mounts` (volume, path, ro/rw), `binds`
- logs

And these across the system:

- **Links:** `from`, `to`, `port`, and `declared` vs. `observed`.
- **Images:** layers with digest, the command that created them, and size.
- **Volumes:** size and files.
- **Networks:** subnet, gateway, bridge device, and whether the network is internal.
