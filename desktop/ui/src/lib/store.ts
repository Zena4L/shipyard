// The app's live store (step 14.2): where the VM and the API stand, and
// what's loaded. Loaded whole, then kept by the API's events: the stream is
// followed before the first list (what happens between the two is applied
// after it), every reconnect loads everything again (the journal isn't in
// time order: a `since` alone could miss an end), and the list is read
// again every 30 s to correct anything missed, the stream left as it is (a
// failed read keeps what's shown, said stale). A container's state comes
// from the list and events alone; the links answer is for links. A lost
// stream is followed again only once the VM serves, never by starting it,
// and the sooner a stream ends, the longer the wait before the next.
import { get, writable, type Readable } from "svelte/store";
import type {
  Backend,
  Container,
  EngineEvent,
  Failure,
  Info,
  Inspected,
  Isolation,
  Links,
  Network,
  NetworkView,
  RunSpec,
  StoredImage,
  VolumesAnswer,
  Sample,
  Snapshot,
  Status,
  Usage,
} from "./backend";
import type { Act } from "./actions";
import { isBridge } from "./graph";
import { liveOf, type LiveNumbers } from "./usage";

/** The rail's host block and the status bar's words, as the mock writes them. */
export interface Host {
  label: string;
  kind: string;
  memory: string;
  memoryShare: number;
  kernel: string;
  backend: string;
  /** Its containers' root is a user's (the inspector's user mapping). */
  rootless: boolean;
  /** On a Linux host, rootless: what the window shows is the user's own
   * engine, never root's (D14.7-1). */
  ownOnly: string | null;
  version: string;
}

/** `info` (Docker's) as the mock's rail and status bar say it, on
 * `platform`, `used` bytes of memory in use if known. */
export function hostOf(info: Info, platform: string | undefined, used: number | null | undefined): Host {
  const arch = info.Architecture === "aarch64" ? "arm64" : info.Architecture === "x86_64" ? "amd64" : (info.Architecture ?? "");
  const total = info.MemTotal ?? 0;
  const rootless = (info.SecurityOptions ?? []).some((option) => option.includes("rootless"));
  return {
    label: platform === "macos" ? "Linux VM" : "Linux",
    kind: platform === "macos" ? `vz · ${arch}` : `host · ${arch}`,
    ...memoryOf(used, total),
    kernel: (info.KernelVersion ?? "—").split("-")[0] ?? "—",
    backend: [rootless ? "rootless" : "root", info.CgroupVersion ? `cgroup v${info.CgroupVersion}` : null, info.Driver]
      .filter(Boolean)
      .join(" · "),
    rootless,
    ownOnly: platform !== "macos" && rootless ? "Your own containers: root's (sudo shipyard) aren't shown here." : null,
    version: info.ServerVersion ? `shipyard ${info.ServerVersion}` : "",
  };
}

/** The rail's memory line and bar: `used` bytes of `total`. */
export function memoryOf(used: number | null | undefined, total: number): Pick<Host, "memory" | "memoryShare"> {
  const gib = (bytes: number) => (bytes / 1024 ** 3).toFixed(1);
  return {
    memory: `${used != null ? gib(used) : "—"} / ${total ? gib(total) : "—"} GiB`,
    memoryShare: used != null && total ? Math.round((used / total) * 100) : 0,
  };
}

/** `error` (a rejection: the core's failure, or anything else) as a
 * failure. */
export function failureOf(error: unknown): Failure {
  if (typeof error === "object" && error !== null && "kind" in error && "message" in error) {
    return { kind: String(error.kind) as Failure["kind"], message: String(error.message) };
  }
  return { kind: "other", message: String(error) };
}

/** What the window heads a failure of each kind with. */
export const FAILURE_HEAD: Record<Failure["kind"], string> = {
  unreachable: "Shipyard can't be reached",
  newer: "This app is older than Shipyard: update it",
  older: "Shipyard is older than this app",
  api: "Shipyard's API refused",
  timeout: "Shipyard didn't answer in time",
  "vm-start": "Shipyard's VM didn't start",
  shipyard: "The shipyard program can't run the VM",
  settings: "The app's settings",
  ended: "The connection to Shipyard ended",
  other: "Something went wrong",
};

/** Where the app stands. */
export type Phase =
  | "checking" // asking the core
  | "stopped" // the VM is stopped (the setting off, or a stop of the user's)
  | "starting" // the VM is starting
  | "connecting" // loading from the API
  | "live" // loaded, following events
  | "failed"; // something to say, and a way to try again

/** The store's state. */
export interface State {
  phase: Phase;
  status: Status | null;
  /** What the VM's start said, last lines. */
  lines: string[];
  /** Containers by ID. */
  containers: Map<string, Container>;
  links: Links | null;
  /** Every network, in the API's order. */
  networks: Network[];
  /** The rail's and status bar's facts. */
  host: Host | null;
  counts: Snapshot["counts"] | null;
  /** The live numbers (`/shipyard/usage`, read every USAGE_MS): each
   * running container's (and its monitor's), the host's memory in use. */
  live: LiveNumbers["live"] | null;
  memoryUsed: number | null;
  /** What to say when it failed. */
  error: Failure | null;
  /** A background read that failed: what's shown is from before it. */
  stale: Failure | null;
  /** A settings change that failed. */
  settingsError: Failure | null;
  /** An action that failed (the map's Start, the inspector's buttons). */
  actionError: Failure | null;
  /** The containers an action is under way on: their buttons wait. */
  busy: Set<string>;
  /** The Run sheet's run, while one is under way or once it failed. */
  run: Run | null;
  /** Every image with its layers, once read (the Images section, the
   * map's overlay, the inspector's image). */
  images: StoredImage[] | null;
  /** Every volume, once read (the Volumes section). */
  volumes: VolumesAnswer | null;
  /** Shipyard's own view of each bridge network, by name, once read (the
   * Networks section): `null` where bridges aren't served (rootless). */
  networkViews: Map<string, NetworkView | null>;
}

/** A run from the Run sheet. */
export interface Run {
  /** The image it runs. */
  image: string;
  /** Under way (else ended with `error`). */
  going: boolean;
  /** Its pull's last line, if it pulled. */
  line: string | null;
  error: Failure | null;
}

/** What the inspector reads of the selected container. */
export interface Focus {
  id: string;
  /** Its isolation, while it runs. */
  isolation: Isolation | null;
  inspected: Inspected | null;
  /** Its last samples, oldest first (at most `SAMPLES`). */
  samples: Sample[];
  /** Why its stats stream ended on its own, if it did. */
  ended: string | null;
}

/** The console's logs: the selection's, its last lines, each numbered
 * as it came (the console keys its rows by it). */
export interface Logs {
  id: string;
  lines: { n: number; text: string }[];
}

/** How many log lines the console keeps. */
export const LOG_LINES = 2000;

/** How many stats samples the inspector keeps (its sparklines' width). */
export const SAMPLES = 48;

/** How often everything is loaded again. */
export const RESYNC_MS = 30_000;

/** How often the map reads the links again (their live connections). */
export const LINKS_MS = 2_000;

/** How often the live numbers are read again (the rail, the Manifest,
 * the status bar). */
export const USAGE_MS = 2_000;

/** How long a hidden window keeps its streams. */
export const HIDDEN_MS = 5 * 60_000;

/** How long a VM that's stopping or booting is waited for, after a lost
 * stream, before saying so. */
export const SETTLE_MS = 60_000;

/** The longest wait before following again, after a stream that ended at
 * once. */
export const BACKOFF_MAX_MS = 30_000;

/** A stream that lived this long resets the wait. */
const LIVED_MS = 60_000;

/** The most start lines kept. */
const KEPT_LINES = 30;

/** A container's state as an event leaves it, or `undefined` to drop it,
 * or `null` when the event says nothing of it (a reload settles it). */
export function stateAfter(action: string): string | undefined | null {
  switch (action) {
    case "start":
    case "unpause":
    case "restart":
      return "running";
    case "pause":
      return "paused";
    case "die":
    case "stop":
      return "exited";
    case "create":
      return "created";
    case "destroy":
      return undefined;
    default:
      return null;
  }
}

/** `containers` with `event` applied: whether it was (else a reload is
 * needed: an event for a container the list doesn't have, or one whose
 * meaning isn't a state). */
export function applyEvent(
  containers: Map<string, Container>,
  event: EngineEvent,
): { containers: Map<string, Container>; applied: boolean } {
  const id = event.Actor?.ID;
  if (event.Type !== "container" || !id || !event.Action) {
    return { containers, applied: event.Type !== "container" };
  }
  const after = stateAfter(event.Action.split(":")[0] ?? "");
  if (after === null) return { containers, applied: true };
  const next = new Map(containers);
  if (after === undefined) {
    next.delete(id);
    return { containers: next, applied: true };
  }
  const known = next.get(id);
  if (!known) return { containers, applied: false };
  next.set(id, { ...known, State: after });
  return { containers: next, applied: true };
}

/** The store over `backend`. */
export function createStore(backend: Backend): Readable<State> & {
  /** The core's events listened to. */
  ready: Promise<void>;
  open(): Promise<void>;
  startVm(): Promise<void>;
  setStartVmOnOpen(start: boolean): Promise<void>;
  chooseShipyard(): Promise<void>;
  setVisible(visible: boolean): void;
  /** Whether the map is shown: its links are read every `LINKS_MS` while
   * it is and the window is visible. */
  watchLinks(on: boolean): void;
  /** Starts a container (the map's fix), its failure said. */
  startContainer(id: string): Promise<boolean>;
  /** An action on a container (see `act`). */
  act(id: string, action: Act, force?: boolean): Promise<boolean>;
  /** A run from the Run sheet (see `run`). */
  run(spec: RunSpec): Promise<string | null>;
  /** The Run sheet's failure forgotten (closed, or edited). */
  clearRun(): void;
  /** Images removed, never forced, one after another (each by its tags, as
   * `docker rmi tag…`: every tag but the last untagged, the last checked
   * by the engine, which refuses an image a container uses; an untagged
   * one by its ID); whether all were. */
  removeImages(images: { id: string; tags: string[] }[]): Promise<boolean>;
  /** Volumes deleted by name, never forced, one after another (the
   * engine refuses one a container names); whether all were. */
  removeVolumes(names: string[]): Promise<boolean>;
  /** The volumes read again (their section shown). */
  readVolumes(): Promise<void>;
  /** Whether the Networks section is shown: the bridges' own views are
   * read while it is (shown, at once). */
  watchNetworks(shown: boolean): void;
  /** The selected container's inspector data, while one is selected. */
  focus: Readable<Focus | null>;
  /** The selected container's logs, while one is selected. */
  logs: Readable<Logs | null>;
  /** Selects a container for the inspector (see `focusOn`). */
  focusOn(id: string | null): Promise<void>;
  close(): void;
} {
  const store = writable<State>({
    phase: "checking",
    status: null,
    lines: [],
    containers: new Map(),
    links: null,
    networks: [],
    host: null,
    counts: null,
    live: null,
    memoryUsed: null,
    error: null,
    stale: null,
    settingsError: null,
    actionError: null,
    busy: new Set(),
    run: null,
    images: null,
    volumes: null,
    networkViews: new Map(),
  });
  // The inspector's data, a store of its own: a sample a second rebuilds
  // the inspector alone, never the map's model.
  const focused = writable<Focus | null>(null);
  // The console's logs, likewise: a busy container's lines rebuild it alone.
  const logged = writable<Logs | null>(null);
  const { subscribe, update } = store;
  let pending: EngineEvent[] | null = null;
  /** Loads begun: a poll's answer overtaken by a load is dropped. */
  let loads = 0;
  /** Whether the Networks section is shown (its views read). */
  let networksShown = false;
  let resync: ReturnType<typeof setInterval> | null = null;
  let hideTimer: ReturnType<typeof setTimeout> | null = null;
  const unlisten: (() => void)[] = [];
  /** The streams let go while the window is hidden: nothing follows. */
  let released = false;
  /** When the stream was last followed, and how many ended soon after. */
  let followedAt = 0;
  let quickEnds = 0;

  const phase = () => get(store).phase;
  const fail = (error: unknown) =>
    update((state) => ({ ...state, phase: "failed", error: failureOf(error) }));
  const sleep = (ms: number) => new Promise((resolve) => setTimeout(resolve, ms));

  /** Loads everything once: with `follow`, the event stream (re)started
   * first. Events that come while the list is read are kept in `pending`
   * and applied after it. */
  async function loadOnce(follow: boolean) {
    if (released) return;
    loads += 1;
    pending = [];
    try {
      if (follow) {
        update((state) => ({ ...state, phase: "connecting", error: null }));
        await backend.follow();
        followedAt = Date.now();
      }
      const snapshot = await backend.snapshot();
      // Let go meanwhile (the hidden window's timer): this answer is dropped,
      // never made live again, so nothing reopens while it's hidden.
      if (released) return;
      memoryTotal = snapshot.info.MemTotal ?? memoryTotal;
      const buffered = pending;
      update((state) => {
        let containers = new Map(snapshot.containers.map((c) => [c.Id, c]));
        for (const event of buffered) containers = applyEvent(containers, event).containers;
        return {
          ...state,
          phase: "live",
          error: null,
          stale: null,
          containers,
          links: snapshot.links,
          networks: snapshot.networks,
          host: hostOf(snapshot.info, state.status?.platform, state.memoryUsed),
          counts: snapshot.counts,
        };
      });
      // A new connection: the selection's stats and isolation are read
      // again (a release or a lost stream ended them; it may have
      // restarted meanwhile).
      const focus = follow ? get(focused) : null;
      if (focus) void focusOn(focus.id);
      if (follow) {
        void loadImages();
        void loadVolumes();
        // A new connection: its first share from reads of its own.
        lastUsage = null;
        void readUsage();
      }
      // Each list read: its networks' views too, while they're shown.
      if (networksShown) void loadNetworks();
    } finally {
      pending = null;
    }
  }

  // One load at a time: one asked while another runs runs once after it
  // (asks meanwhile share it, following if any of them asked to).
  let running: Promise<void> | null = null;
  let queued: Promise<void> | null = null;
  let queuedFollow = false;
  function load(follow: boolean): Promise<void> {
    if (!running) {
      running = loadOnce(follow).finally(() => (running = null));
      return running;
    }
    queuedFollow ||= follow;
    queued ??= running
      .catch(() => {})
      .then(() => {
        const again = queuedFollow;
        queued = null;
        queuedFollow = false;
        return load(again);
      });
    return queued;
  }

  /** The list read again in the background: on a failure, what's shown
   * stays, said stale (the next resync tries again). */
  function refresh() {
    void load(false).catch((error) => update((state) => ({ ...state, stale: failureOf(error) })));
  }

  async function open() {
    await fresh;
    released = false;
    usageUnserved = false;
    poll();
    try {
      const status = await backend.status();
      update((state) => ({ ...state, status }));
      if (status.vm && status.vm.state === "stopped") {
        if (!status.startVmOnOpen) {
          update((state) => ({ ...state, phase: "stopped" }));
          return;
        }
        await startVm();
        return;
      }
      if (status.vm && status.vm.state === "running" && !status.vm.serving) {
        update((state) => ({ ...state, phase: "starting" }));
        await backend.startVm();
      }
      await load(true);
    } catch (error) {
      fail(error);
    }
  }

  async function startVm() {
    try {
      update((state) => ({ ...state, phase: "starting", lines: [], error: null }));
      await backend.startVm();
      await load(true);
    } catch (error) {
      fail(error);
    }
  }

  /** After a lost stream: waits (longer the sooner streams end), then
   * follows again once the VM serves; a VM stopping or stopped ends at
   * `stopped`, never started again from here. */
  async function reconnect() {
    quickEnds = Date.now() - followedAt >= LIVED_MS ? 0 : quickEnds + 1;
    update((state) => ({ ...state, phase: "checking" }));
    if (quickEnds > 0) await sleep(Math.min(BACKOFF_MAX_MS, 1000 * 2 ** (quickEnds - 1)));
    const began = Date.now();
    for (;;) {
      if (released) return;
      const status = await backend.status();
      update((state) => ({ ...state, status }));
      const vm = status.vm;
      if (vm && vm.state === "running" && vm.phase === "stopping") {
        update((state) => ({ ...state, phase: "stopped" }));
        return;
      }
      if (vm && vm.state === "stopped") {
        update((state) => ({ ...state, phase: "stopped" }));
        return;
      }
      if (!vm || vm.serving) break;
      if (Date.now() - began > SETTLE_MS) throw new Error(`Shipyard's VM is still ${vm.phase}`);
      await sleep(1000);
    }
    await load(true);
  }

  async function changeSettings(change: () => Promise<void>) {
    try {
      await change();
      update((state) => ({ ...state, settingsError: null }));
    } catch (error) {
      update((state) => ({ ...state, settingsError: failureOf(error) }));
    }
    try {
      const status = await backend.status();
      update((state) => ({ ...state, status }));
    } catch {
      // What's shown stays; the next open asks again.
    }
  }

  // What an earlier page left in the core (a reload, a crashed web content
  // process): its streams and shell let go, its selections' numbering begun
  // again, before this page opens anything (`open` waits for it).
  const fresh = backend.release().catch(() => {});
  const ready = Promise.all([
    backend.onEvent((event) => {
      if (pending) {
        pending.push(event);
        return;
      }
      // An image made, removed or tagged: the images read again.
      if (event.Type === "image") void loadImages();
      // A volume made or deleted: the volumes read again.
      if (event.Type === "volume") void loadVolumes();
      // A start's new veth, while the networks are shown.
      if (networksShown && event.Type === "container" && event.Action === "start") void loadNetworks();
      // A container's end read again: one with a restart policy is
      // restarting, not exited (and its Remove must stop it first). A
      // network or volume made, removed, joined or left: the list read
      // again (the networks, each container's, the rail's counts).
      let reload = (event.Type === "container" && event.Action === "die") || event.Type === "network" || event.Type === "volume";
      update((state) => {
        const { containers, applied } = applyEvent(state.containers, event);
        reload ||= !applied;
        return { ...state, containers };
      });
      // The stream is up: the list alone again.
      if (reload) refresh();
    }),
    backend.onVmLine((line) =>
      update((state) => ({ ...state, lines: [...state.lines, line].slice(-KEPT_LINES) })),
    ),
    backend.onStats((sample) =>
      focused.update((focus) =>
        focus && focus.id === sample.id ? { ...focus, samples: [...focus.samples, sample].slice(-SAMPLES) } : focus,
      ),
    ),
    backend.onRunLine((line) =>
      update((state) => (state.run?.going ? { ...state, run: { ...state.run, line } } : state)),
    ),
    backend.onStatsEnded((ended) =>
      focused.update((focus) => (focus && focus.id === ended.id ? { ...focus, ended: ended.why } : focus)),
    ),
    backend.onLost(() => {
      // Only a stream the window follows: never one let go, or after a
      // failure (Try again opens anew).
      if (released || (phase() !== "live" && phase() !== "connecting")) return;
      void reconnect().catch(fail);
    }),
  ]).then((handles) => {
    unlisten.push(...handles);
  });

  resync = setInterval(() => {
    if (phase() === "live") refresh();
  }, RESYNC_MS);

  // The polls: the map's links read again every LINKS_MS while it's shown,
  // the window visible and the app live, one read at a time; the live
  // numbers' beside it (`pollUsage`).
  let watching = false;
  let visible = true;
  let linksTimer: ReturnType<typeof setInterval> | null = null;
  let reading = false;
  function poll() {
    pollUsage();
    const want = watching && visible && !released;
    if (want && !linksTimer) {
      linksTimer = setInterval(() => {
        if (reading || phase() !== "live") return;
        reading = true;
        const began = loads;
        backend
          .links()
          .then((links) => {
            if (loads === began) update((state) => ({ ...state, links, stale: null }));
          })
          .catch((error) => update((state) => ({ ...state, stale: failureOf(error) })))
          .finally(() => (reading = false));
      }, LINKS_MS);
    } else if (!want && linksTimer) {
      clearInterval(linksTimer);
      linksTimer = null;
    }
  }

  // The live numbers' poll: every USAGE_MS while the window is seen and
  // live, one read at a time; a CPU share needs the read before.
  let usageTimer: ReturnType<typeof setInterval> | null = null;
  let usageReading = false;
  let lastUsage: Usage | null = null;
  /** The host's memory, bytes, as the last list or usage said it. */
  let memoryTotal = 0;
  /** The Shipyard that answers doesn't serve the usage: not asked again. */
  let usageUnserved = false;
  function pollUsage() {
    const want = visible && !released;
    if (want && !usageTimer) usageTimer = setInterval(() => void readUsage(), USAGE_MS);
    else if (!want && usageTimer) {
      clearInterval(usageTimer);
      usageTimer = null;
      // Shares across a pause would be one long average: begin again.
      lastUsage = null;
    }
  }

  async function readUsage() {
    if (usageReading || usageUnserved || released || phase() !== "live") return;
    usageReading = true;
    try {
      const usage = await backend.usage();
      // The API's shared answer again (within its second): nothing to
      // divide by; the next read will have moved on.
      if (released || (lastUsage && usage.cpu.time === lastUsage.cpu.time)) return;
      const numbers = liveOf(usage, lastUsage);
      lastUsage = usage;
      memoryTotal = usage.memory.total || memoryTotal;
      update((state) => ({
        ...state,
        ...numbers,
        host: state.host && { ...state.host, ...memoryOf(numbers.memoryUsed, memoryTotal) },
      }));
    } catch (error) {
      // A read that failed once (a slow API) keeps what's shown, the list's
      // reads say a lost connection; a Shipyard of another version, which
      // won't answer, shows none ("—") and isn't asked again until the
      // window opens anew.
      const kind = failureOf(error).kind;
      if (kind === "older" || kind === "newer") {
        usageUnserved = true;
        lastUsage = null;
        update((state) => ({
          ...state,
          live: null,
          memoryUsed: null,
          host: state.host && { ...state.host, ...memoryOf(null, memoryTotal) },
        }));
      }
    } finally {
      usageReading = false;
    }
  }

  function watchLinks(on: boolean) {
    watching = on;
    poll();
  }

  const asked: Record<Act, (id: string, force: boolean) => Promise<void>> = {
    start: (id) => backend.startContainer(id),
    stop: (id) => backend.stopContainer(id),
    pause: (id) => backend.pauseContainer(id),
    resume: (id) => backend.unpauseContainer(id),
    restart: (id) => backend.restartContainer(id),
    remove: (id, force) => backend.removeContainer(id, force),
  };

  /** `action` on container `id` (`force`: a remove that stops it first),
   * one at a time a container; its failure said, the list read again
   * after. Whether it was done. */
  async function act(id: string, action: Act, force = false): Promise<boolean> {
    if (get(store).busy.has(id)) return false;
    update((state) => ({ ...state, busy: new Set(state.busy).add(id) }));
    try {
      await asked[action](id, force);
      update((state) => ({ ...state, actionError: null }));
      refresh();
      return true;
    } catch (error) {
      update((state) => ({ ...state, actionError: failureOf(error) }));
      return false;
    } finally {
      update((state) => {
        const busy = new Set(state.busy);
        busy.delete(id);
        return { ...state, busy };
      });
    }
  }

  /** `read`, one at a time: one asked meanwhile reads again after it; a
   * failure said as stale, what's shown kept. */
  function oneAtATime(read: () => Promise<void>): () => Promise<void> {
    let reading: Promise<void> | null = null;
    let again = false;
    // The failure this reader said, cleared once it reads again (never
    // another reader's, or a load's).
    let said: Failure | null = null;
    return () => {
      if (reading) {
        again = true;
        return reading;
      }
      reading = (async () => {
        do {
          again = false;
          try {
            await read();
            if (said) {
              const mine = said;
              said = null;
              update((state) => (state.stale === mine ? { ...state, stale: null } : state));
            }
          } catch (error) {
            said = failureOf(error);
            const failure = said;
            update((state) => ({ ...state, stale: failure }));
          }
        } while (again);
      })().finally(() => (reading = null));
      return reading;
    };
  }

  // The images, the volumes, the bridge networks' own views.
  const loadImages = oneAtATime(async () => {
    const images = await backend.images();
    update((state) => ({ ...state, images }));
  });
  const loadVolumes = oneAtATime(async () => {
    const volumes = await backend.volumes();
    update((state) => ({ ...state, volumes }));
  });
  const loadNetworks = oneAtATime(async () => {
    const names = get(store)
      .networks.filter(isBridge)
      .map((network) => network.name);
    const before = get(store).networkViews;
    const views = new Map<string, NetworkView | null>();
    // One that fails keeps what it said, the first failure said; the rest stand.
    let failed: unknown = null;
    for (const name of names) {
      try {
        views.set(name, await backend.networkView(name));
      } catch (error) {
        failed ??= error;
        if (before.has(name)) views.set(name, before.get(name) ?? null);
      }
    }
    update((state) => ({ ...state, networkViews: views, ...(failed ? { stale: failureOf(failed) } : {}) }));
  });

  /** `change`, its failure said, then `reload`. Whether it was done. */
  async function changeThen(change: () => Promise<unknown>, reload: () => Promise<void>): Promise<boolean> {
    try {
      await change();
      update((state) => ({ ...state, actionError: null }));
      return true;
    } catch (error) {
      update((state) => ({ ...state, actionError: failureOf(error) }));
      return false;
    } finally {
      void reload();
    }
  }

  /** Runs `spec` from the Run sheet (one run at a time): the new
   * container's ID, or null with the run's failure kept. */
  async function run(spec: RunSpec): Promise<string | null> {
    if (get(store).run?.going) return null;
    update((state) => ({ ...state, run: { image: spec.image, going: true, line: null, error: null } }));
    try {
      const id = await backend.runImage(spec);
      update((state) => ({ ...state, run: null }));
      refresh();
      return id;
    } catch (error) {
      update((state) => ({ ...state, run: { image: spec.image, going: false, line: null, error: failureOf(error) } }));
      return null;
    }
  }

  function setVisible(shown: boolean) {
    visible = shown;
    poll();
    if (shown) {
      if (hideTimer) clearTimeout(hideTimer);
      hideTimer = null;
      if (released) void open();
      return;
    }
    // Hidden a while: its streams let go (the VM's idle stop may come);
    // shown again, opened anew. A window showing a stopped VM or a failure
    // holds nothing to let go of, and is shown as it was.
    hideTimer ??= setTimeout(() => {
      hideTimer = null;
      void backend.release();
      const now = phase();
      if (now === "live" || now === "connecting" || now === "starting" || now === "checking") {
        released = true;
        poll();
        update((state) => ({ ...state, phase: "checking" }));
      }
    }, HIDDEN_MS);
  }

  /** Each selection's number: what a slower, older one finds changed. */
  let selection = 0;

  /** Selects container `id` (or none) for the inspector, as the list has
   * it now: its stats followed and its isolation read while it runs, its
   * inspect and image read; the last selection's let go. A later call (a
   * new selection, or this one's start or stop) overtakes it. */
  async function focusOn(id: string | null) {
    const mine = ++selection;
    void backend.unwatchStats(mine).catch(() => {});
    const container = id ? get(store).containers.get(id) : undefined;
    if (!id) {
      // Nothing follows this one (a follow would stop the last itself).
      void backend.logsStop(mine).catch(() => {});
      focused.set(null);
      logged.set(null);
      return;
    }
    const running = container?.State === "running";
    focused.set({ id, isolation: null, inspected: null, samples: [], ended: null });
    // Its logs: its last lines, then each new one while it runs, a batch
    // at a time (a later selection's, even of the same container, drops
    // what's still on its way to this one).
    logged.set({ id, lines: [] });
    let n = 0;
    const take = (texts: string[]) =>
      logged.update((logs) =>
        logs && mine === selection ? { id, lines: [...logs.lines, ...texts.map((text) => ({ n: n++, text }))].slice(-LOG_LINES) } : logs,
      );
    void backend
      .logsFollow(id, running, mine, (batch) => take(batch.ended ? [...batch.lines, `[the logs ended: ${batch.ended}]`] : batch.lines))
      .catch((error) => take([`[no logs: ${failureOf(error).message}]`]));
    const [isolation, inspected] = await Promise.all([
      running ? backend.isolation(id).catch(() => null) : null,
      backend.inspect(id).catch(() => null),
    ]);
    if (mine !== selection) return;
    focused.update((focus) => (focus ? { ...focus, isolation, inspected } : focus));
    if (running) await backend.watchStats(id, mine).catch(() => {});
  }

  function close() {
    if (resync) clearInterval(resync);
    if (linksTimer) clearInterval(linksTimer);
    if (usageTimer) clearInterval(usageTimer);
    if (hideTimer) clearTimeout(hideTimer);
    for (const handle of unlisten) handle();
  }

  return {
    subscribe,
    focus: { subscribe: focused.subscribe },
    logs: { subscribe: logged.subscribe },
    ready,
    open,
    startVm,
    setStartVmOnOpen: (start) => changeSettings(() => backend.setStartVmOnOpen(start)),
    chooseShipyard: () => changeSettings(() => backend.chooseShipyard()),
    setVisible,
    watchLinks,
    startContainer: (id) => act(id, "start"),
    act,
    run,
    removeImages: (images) =>
      changeThen(async () => {
        for (const image of images) {
          for (const reference of image.tags.length ? image.tags : [image.id]) await backend.removeImage(reference, false);
        }
      }, loadImages),
    removeVolumes: (names) =>
      changeThen(async () => {
        for (const name of names) await backend.removeVolume(name);
      }, loadVolumes),
    readVolumes: () => loadVolumes(),
    watchNetworks: (shown) => {
      networksShown = shown;
      if (shown) void loadNetworks();
    },
    clearRun: () => update((state) => (state.run?.going ? state : { ...state, run: null })),
    focusOn,
    close,
  };
}
