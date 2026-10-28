import { describe, expect, it, vi } from "vitest";
import type { Backend, Container, EngineEvent, Links, LogBatch, Sample, Snapshot, StatsEnded } from "./backend";
import { listed } from "./model";
import {
  HIDDEN_MS,
  LINKS_MS,
  LOG_LINES,
  RESYNC_MS,
  SAMPLES,
  USAGE_MS,
  applyEvent,
  createStore,
  failureOf,
  hostOf,
  type Focus,
  type Logs,
  type State,
} from "./store";

const container = (id: string, state: string): Container => ({ Id: id, Names: [`/${id}`], State: state });

/** A snapshot of `containers`, nothing else in it. */
const snap = (containers: Container[]): Snapshot => ({
  containers,
  links: { version: 1, containers: [], links: [], observed: { available: true, cause: null, reason: null } },
  info: {},
  counts: { images: 0, volumes: 0, networks: 0 },
  networks: [],
});

describe("hostOf", () => {
  it("says a rootless Linux host shows the user's own containers, never root's", () => {
    const rootless = { SecurityOptions: ["name=rootless", "name=cgroupns"], Architecture: "aarch64" };
    expect(hostOf(rootless, "linux", null).ownOnly).toBe("Your own containers: root's (sudo shipyard) aren't shown here.");
    expect(hostOf(rootless, "linux", null).backend).toBe("rootless");
    // Root sees root's; the Mac's VM is its own.
    expect(hostOf({ SecurityOptions: ["name=seccomp"] }, "linux", null).ownOnly).toBeNull();
    expect(hostOf(rootless, "macos", null).ownOnly).toBeNull();
  });
});

describe("applyEvent", () => {
  it("applies a container's state, drops a destroyed one, asks a reload for one it doesn't know", () => {
    const known = new Map([["a", container("a", "running")]]);
    const died = applyEvent(known, { Type: "container", Action: "die", Actor: { ID: "a" } });
    expect(died.applied).toBe(true);
    expect(died.containers.get("a")?.State).toBe("exited");
    expect(known.get("a")?.State).toBe("running");
    const gone = applyEvent(known, { Type: "container", Action: "destroy", Actor: { ID: "a" } });
    expect(gone.containers.has("a")).toBe(false);
    const unknown = applyEvent(known, { Type: "container", Action: "start", Actor: { ID: "b" } });
    expect(unknown.applied).toBe(false);
    // A signal isn't an end: `die` says the end.
    const signalled = applyEvent(known, { Type: "container", Action: "kill", Actor: { ID: "a" } });
    expect(signalled.containers.get("a")?.State).toBe("running");
    const exec = applyEvent(known, { Type: "container", Action: "exec_start: sh", Actor: { ID: "a" } });
    expect(exec.applied).toBe(true);
    expect(exec.containers.get("a")?.State).toBe("running");
  });
});

/** A backend whose VM runs and serves, with `containers`; its handlers kept. */
function fakeBackend(containers: Container[]) {
  const handlers: {
    event?: (event: EngineEvent) => void;
    lost?: (why: string) => void;
    stats?: (sample: Sample) => void;
    statsEnded?: (ended: StatsEnded) => void;
    runLine?: (line: string) => void;
    logBatch?: (batch: LogBatch) => void;
  } = {};
  const backend: Backend = {
    status: vi.fn(async () => ({
      platform: "macos",
      vm: { state: "running" as const, phase: "up", serving: true },
      startVmOnOpen: true,
      shipyard: { path: "/usr/local/bin/shipyard" },
      settingsSaid: null,
    })),
    startVm: vi.fn(async () => {}),
    snapshot: vi.fn(async () => snap(containers)),
    links: vi.fn(async () => snap(containers).links),
    usage: vi.fn(async () => ({
      memory: { total: 0, available: 0 },
      cpu: { time: 0, cpus: 1 },
      containers: [],
    })),
    startContainer: vi.fn(async () => {}),
    stopContainer: vi.fn(async () => {}),
    restartContainer: vi.fn(async () => {}),
    pauseContainer: vi.fn(async () => {}),
    unpauseContainer: vi.fn(async () => {}),
    removeContainer: vi.fn(async () => {}),
    runImage: vi.fn(async () => "new"),
    images: vi.fn(async () => []),
    removeImage: vi.fn(async () => {}),
    volumes: vi.fn(async () => ({ version: 1, volumes: [], warnings: [] })),
    removeVolume: vi.fn(async () => {}),
    networkView: vi.fn(async () => null),
    onRunLine: vi.fn(async (handler) => {
      handlers.runLine = handler;
      return () => {};
    }),
    logsFollow: vi.fn(async (_id: string, _follow: boolean, _selection: number, onbatch: (batch: LogBatch) => void) => {
      handlers.logBatch = onbatch;
    }),
    logsStop: vi.fn(async () => {}),
    shellOpen: vi.fn(async () => 1),
    shellInput: vi.fn(async () => {}),
    shellResize: vi.fn(async () => {}),
    shellClose: vi.fn(async () => {}),
    shellAck: vi.fn(async () => {}),
    isolation: vi.fn(async () => {
      throw { kind: "api", message: "not running" };
    }),
    inspect: vi.fn(async () => ({})),
    watchStats: vi.fn(async () => {}),
    unwatchStats: vi.fn(async () => {}),
    onStats: vi.fn(async (handler) => {
      handlers.stats = handler;
      return () => {};
    }),
    onStatsEnded: vi.fn(async (handler) => {
      handlers.statsEnded = handler;
      return () => {};
    }),
    follow: vi.fn(async () => {}),
    release: vi.fn(async () => {}),
    setStartVmOnOpen: vi.fn(async () => {}),
    chooseShipyard: vi.fn(async () => {}),
    onEvent: vi.fn(async (handler) => {
      handlers.event = handler;
      return () => {};
    }),
    onVmLine: vi.fn(async () => () => {}),
    onLost: vi.fn(async (handler) => {
      handlers.lost = handler;
      return () => {};
    }),
  };
  return { backend, handlers };
}

describe("the store", () => {
  it("follows events before it lists, and applies those that came between", async () => {
    const { backend, handlers } = fakeBackend([container("a", "running")]);
    backend.snapshot = vi.fn(async () => {
      // An event that arrives while the list is being read.
      handlers.event?.({ Type: "container", Action: "die", Actor: { ID: "a" } });
      return snap([container("a", "running")]);
    });
    const store = createStore(backend);
    await new Promise((resolve) => setTimeout(resolve));
    await store.open();
    let state = { phase: "", containers: new Map<string, Container>() };
    store.subscribe((value) => (state = value))();
    expect(backend.follow).toHaveBeenCalledBefore(backend.snapshot as never);
    expect(state.phase).toBe("live");
    expect(state.containers.get("a")?.State).toBe("exited");
    store.close();
  });

  it("runs a load asked during another after it, never beside it", async () => {
    const { backend } = fakeBackend([container("a", "running")]);
    let release = () => {};
    const held = new Promise<void>((resolve) => (release = resolve));
    let reads = 0;
    backend.snapshot = vi.fn(async () => {
      reads += 1;
      if (reads === 1) await held;
      return snap([container("a", "running")]);
    });
    const store = createStore(backend);
    await store.ready;
    const opened = store.open();
    await vi.waitFor(() => expect(reads).toBe(1));
    // Opened again while the first list is read: a second load asked.
    const again = store.open();
    await new Promise((resolve) => setTimeout(resolve));
    expect(reads).toBe(1);
    release();
    await Promise.all([opened, again]);
    await vi.waitFor(() => expect(reads).toBe(2));
    let state: { phase: string; error: unknown } = { phase: "", error: null };
    store.subscribe((value) => (state = value))();
    expect(state.error).toBeNull();
    expect(state.phase).toBe("live");
    expect(backend.follow).toHaveBeenCalledTimes(2);
    store.close();
  });

  it("reads the list again every while, the stream left as it is", async () => {
    vi.useFakeTimers();
    try {
      const { backend } = fakeBackend([container("a", "running")]);
      const store = createStore(backend);
      await vi.advanceTimersByTimeAsync(0);
      await store.open();
      await vi.advanceTimersByTimeAsync(RESYNC_MS);
      expect(backend.snapshot).toHaveBeenCalledTimes(2);
      expect(backend.follow).toHaveBeenCalledTimes(1);
      store.close();
    } finally {
      vi.useRealTimers();
    }
  });

  it("starts a stopped VM when the setting says so", async () => {
    const { backend } = fakeBackend([]);
    const running = backend.status;
    backend.status = vi.fn(async () => ({
      platform: "macos",
      vm: { state: "stopped" as const },
      startVmOnOpen: true,
      shipyard: { path: "/usr/local/bin/shipyard" },
      settingsSaid: null,
    }));
    backend.startVm = vi.fn(async () => {
      backend.status = running;
    });
    const store = createStore(backend);
    await store.open();
    let phase = "";
    store.subscribe((value) => (phase = value.phase))();
    expect(backend.startVm).toHaveBeenCalledTimes(1);
    expect(phase).toBe("live");
    store.close();
  });

  it("starts a stopped VM only when the setting says so", async () => {
    const { backend } = fakeBackend([]);
    backend.status = vi.fn(async () => ({
      platform: "macos",
      vm: { state: "stopped" as const },
      startVmOnOpen: false,
      shipyard: { path: "/usr/local/bin/shipyard" },
      settingsSaid: null,
    }));
    const store = createStore(backend);
    await store.open();
    let phase = "";
    store.subscribe((value) => (phase = value.phase))();
    expect(phase).toBe("stopped");
    expect(backend.startVm).not.toHaveBeenCalled();
    store.close();
  });
});

describe("listed", () => {
  it("keeps the nodes the list still has", () => {
    const nodes = [{ id: "a" }, { id: "b" }];
    expect(listed(nodes, new Map([["b", container("b", "running")]]))).toEqual([{ id: "b" }]);
  });
});

describe("failureOf", () => {
  it("keeps the core's kind, and calls anything else other", () => {
    expect(failureOf({ kind: "newer", message: "the links answer is version 2" })).toEqual({
      kind: "newer",
      message: "the links answer is version 2",
    });
    expect(failureOf("boom")).toEqual({ kind: "other", message: "boom" });
  });
});

/** The store's state now. */
function now(store: { subscribe: (run: (value: State) => void) => () => void }): State {
  let state: State | undefined;
  store.subscribe((value) => (state = value))();
  return state as State;
}

describe("the store, over time", () => {
  it("ends at stopped after a user's stop, starting nothing", async () => {
    vi.useFakeTimers();
    try {
      const { backend, handlers } = fakeBackend([container("a", "running")]);
      const store = createStore(backend);
      await store.ready;
      await store.open();
      expect(now(store).phase).toBe("live");
      // The VM stopping: its stream ended while its host says so.
      backend.status = vi.fn(async () => ({
        platform: "macos",
        vm: { state: "running" as const, phase: "stopping", serving: false },
        startVmOnOpen: true,
        shipyard: { path: "/usr/local/bin/shipyard" },
        settingsSaid: null,
      }));
      handlers.lost?.("ended");
      await vi.advanceTimersByTimeAsync(BACKOFF);
      expect(now(store).phase).toBe("stopped");
      expect(backend.startVm).not.toHaveBeenCalled();
      expect(backend.follow).toHaveBeenCalledTimes(1);
      store.close();
    } finally {
      vi.useRealTimers();
    }
  });

  it("waits longer before following again the sooner each stream ends", async () => {
    vi.useFakeTimers();
    try {
      const { backend, handlers } = fakeBackend([container("a", "running")]);
      const store = createStore(backend);
      await store.ready;
      await store.open();
      handlers.lost?.("ended");
      await vi.advanceTimersByTimeAsync(999);
      expect(backend.follow).toHaveBeenCalledTimes(1);
      await vi.advanceTimersByTimeAsync(1);
      expect(backend.follow).toHaveBeenCalledTimes(2);
      // At once again: twice as long.
      handlers.lost?.("ended");
      await vi.advanceTimersByTimeAsync(1999);
      expect(backend.follow).toHaveBeenCalledTimes(2);
      await vi.advanceTimersByTimeAsync(1);
      expect(backend.follow).toHaveBeenCalledTimes(3);
      expect(now(store).phase).toBe("live");
      store.close();
    } finally {
      vi.useRealTimers();
    }
  });

  it("keeps what it shows when a background read fails, said stale", async () => {
    vi.useFakeTimers();
    try {
      const { backend } = fakeBackend([container("a", "running")]);
      const store = createStore(backend);
      await store.ready;
      await store.open();
      backend.snapshot = vi.fn(async () => {
        throw { kind: "timeout", message: "the links answer took longer than 10 s" };
      });
      await vi.advanceTimersByTimeAsync(RESYNC_MS);
      const state = now(store);
      expect(state.phase).toBe("live");
      expect(state.containers.has("a")).toBe(true);
      expect(state.stale?.kind).toBe("timeout");
      // The next read that works clears it.
      backend.snapshot = vi.fn(async () => snap([container("a", "running")]));
      await vi.advanceTimersByTimeAsync(RESYNC_MS);
      expect(now(store).stale).toBeNull();
      store.close();
    } finally {
      vi.useRealTimers();
    }
  });

  it("drops a load that lands after the window was let go, never live again", async () => {
    vi.useFakeTimers();
    try {
      const { backend } = fakeBackend([container("a", "running")]);
      const store = createStore(backend);
      await store.ready;
      await store.open();
      // A resync's read in flight as the hidden window's timer fires.
      let answer: (value: Snapshot) => void = () => {};
      backend.snapshot = vi.fn(() => new Promise<Snapshot>((resolve) => (answer = resolve)));
      await vi.advanceTimersByTimeAsync(RESYNC_MS);
      store.setVisible(false);
      await vi.advanceTimersByTimeAsync(HIDDEN_MS);
      expect(now(store).phase).toBe("checking");
      answer(snap([container("a", "running")]));
      await vi.advanceTimersByTimeAsync(10);
      expect(now(store).phase).toBe("checking");
      store.close();
    } finally {
      vi.useRealTimers();
    }
  });

  it("lets go of what an earlier page left before it follows anything", async () => {
    const { backend } = fakeBackend([container("a", "running")]);
    const order: string[] = [];
    let released: () => void = () => {};
    backend.release = vi.fn(() => new Promise<void>((resolve) => (released = () => (order.push("release"), resolve()))));
    const follow = backend.follow;
    backend.follow = vi.fn(async () => {
      order.push("follow");
      return follow();
    });
    const store = createStore(backend);
    await store.ready;
    const opening = store.open();
    // Nothing followed while the release is under way.
    await new Promise((resolve) => setTimeout(resolve, 20));
    expect(order).toEqual([]);
    released();
    await opening;
    expect(order).toEqual(["release", "follow"]);
    store.close();
  });

  it("lets go of a hidden window's streams, and opens anew when shown", async () => {
    vi.useFakeTimers();
    try {
      const { backend } = fakeBackend([container("a", "running")]);
      const store = createStore(backend);
      await store.ready;
      await store.open();
      // (Once as the page loaded: what an earlier page left.)
      expect(backend.release).toHaveBeenCalledTimes(1);
      store.setVisible(false);
      await vi.advanceTimersByTimeAsync(HIDDEN_MS);
      expect(backend.release).toHaveBeenCalledTimes(2);
      expect(now(store).phase).toBe("checking");
      // Nothing is read while it's let go (resyncs ran until then).
      const reads = (backend.snapshot as ReturnType<typeof vi.fn>).mock.calls.length;
      await vi.advanceTimersByTimeAsync(RESYNC_MS);
      expect(backend.snapshot).toHaveBeenCalledTimes(reads);
      store.setVisible(true);
      await vi.advanceTimersByTimeAsync(0);
      expect(backend.follow).toHaveBeenCalledTimes(2);
      expect(now(store).phase).toBe("live");
      store.close();
    } finally {
      vi.useRealTimers();
    }
  });

  it("shows a stopped VM as it was after a hide, never starting it", async () => {
    vi.useFakeTimers();
    try {
      const { backend } = fakeBackend([]);
      backend.status = vi.fn(async () => ({
        platform: "macos",
        vm: { state: "stopped" as const },
        startVmOnOpen: false,
        shipyard: { path: "/usr/local/bin/shipyard" },
        settingsSaid: null,
      }));
      const store = createStore(backend);
      await store.ready;
      await store.open();
      // The setting turned on meanwhile: still, a hide and a show start nothing.
      backend.status = vi.fn(async () => ({
        platform: "macos",
        vm: { state: "stopped" as const },
        startVmOnOpen: true,
        shipyard: { path: "/usr/local/bin/shipyard" },
        settingsSaid: null,
      }));
      store.setVisible(false);
      await vi.advanceTimersByTimeAsync(HIDDEN_MS);
      store.setVisible(true);
      await vi.advanceTimersByTimeAsync(0);
      expect(now(store).phase).toBe("stopped");
      expect(backend.startVm).not.toHaveBeenCalled();
      store.close();
    } finally {
      vi.useRealTimers();
    }
  });

  it("reads the live numbers at once and every while the window is seen, the rail's memory and CPU shares from them", async () => {
    vi.useFakeTimers();
    try {
      const { backend } = fakeBackend([container("a", "running")]);
      const MIB = 1024 ** 2;
      let reads = 0;
      backend.usage = vi.fn(async () => {
        reads += 1;
        return {
          memory: { total: 4096 * MIB, available: 3072 * MIB },
          cpu: { time: reads * 4_000, cpus: 4 },
          containers: [{ id: "a", cpu_time: reads * 1_000, memory: 10 * MIB, monitor_memory: 3 * MIB }],
        };
      });
      const store = createStore(backend);
      await store.ready;
      await store.open();
      await vi.advanceTimersByTimeAsync(0);
      // Read once with the list: memory and monitors, no CPU share yet.
      expect(reads).toBe(1);
      expect(now(store).live?.a).toEqual({ cpu: null, mem: 10, monitor: 3 });
      expect(now(store).host?.memory).toBe("1.0 / 4.0 GiB");
      expect(now(store).host?.memoryShare).toBe(25);
      await vi.advanceTimersByTimeAsync(USAGE_MS);
      expect(now(store).live?.a?.cpu).toBe(100);
      // Hidden: no reads; seen again, a share begins anew (none across the gap).
      store.setVisible(false);
      await vi.advanceTimersByTimeAsync(USAGE_MS * 3);
      expect(reads).toBe(2);
      store.setVisible(true);
      await vi.advanceTimersByTimeAsync(USAGE_MS);
      expect(reads).toBe(3);
      expect(now(store).live?.a?.cpu).toBeNull();
      await vi.advanceTimersByTimeAsync(USAGE_MS);
      expect(now(store).live?.a?.cpu).toBe(100);
      // The API's shared answer twice: no share computed from it.
      const working = backend.usage;
      backend.usage = vi.fn(async () => ({
        memory: { total: 4096 * MIB, available: 3072 * MIB },
        cpu: { time: reads * 4_000, cpus: 4 },
        containers: [{ id: "a", cpu_time: reads * 1_000 + 5, memory: 10 * MIB, monitor_memory: 3 * MIB }],
      }));
      await vi.advanceTimersByTimeAsync(USAGE_MS);
      expect(now(store).live?.a?.cpu).toBe(100);
      // A read that fails once (slow): what's shown stays.
      backend.usage = vi.fn(async () => {
        throw { kind: "timeout", message: "slow" };
      });
      await vi.advanceTimersByTimeAsync(USAGE_MS);
      expect(now(store).live?.a?.mem).toBe(10);
      // A Shipyard that doesn't serve it: none shown, never asked again.
      backend.usage = vi.fn(async () => {
        throw { kind: "older", message: "older" };
      });
      await vi.advanceTimersByTimeAsync(USAGE_MS);
      expect(now(store).live).toBeNull();
      expect(now(store).host?.memory).toBe("— / 4.0 GiB");
      await vi.advanceTimersByTimeAsync(USAGE_MS * 3);
      expect(backend.usage).toHaveBeenCalledTimes(1);
      backend.usage = working;
      store.close();
    } finally {
      vi.useRealTimers();
    }
  });

  it("reads the map's links every while, only while the map is shown and the window seen", async () => {
    vi.useFakeTimers();
    try {
      const { backend } = fakeBackend([container("a", "running")]);
      const store = createStore(backend);
      await store.ready;
      await store.open();
      const reads = () => (backend.links as ReturnType<typeof vi.fn>).mock.calls.length;
      await vi.advanceTimersByTimeAsync(LINKS_MS * 3);
      expect(reads()).toBe(0);
      store.watchLinks(true);
      await vi.advanceTimersByTimeAsync(LINKS_MS * 3);
      expect(reads()).toBe(3);
      store.setVisible(false);
      await vi.advanceTimersByTimeAsync(LINKS_MS * 3);
      expect(reads()).toBe(3);
      store.setVisible(true);
      await vi.advanceTimersByTimeAsync(LINKS_MS);
      expect(reads()).toBe(4);
      store.watchLinks(false);
      await vi.advanceTimersByTimeAsync(LINKS_MS * 3);
      expect(reads()).toBe(4);
      store.close();
    } finally {
      vi.useRealTimers();
    }
  });

  it("starts a container, says its failure, and clears it once one works", async () => {
    const { backend } = fakeBackend([container("a", "exited")]);
    const store = createStore(backend);
    await store.ready;
    await store.open();
    const failing = vi.fn(async () => {
      throw { kind: "api", message: "no such container" };
    });
    const working = backend.startContainer;
    backend.startContainer = failing;
    await store.startContainer("a");
    expect(now(store).actionError?.message).toBe("no such container");
    backend.startContainer = working;
    await store.startContainer("a");
    expect(working).toHaveBeenCalledWith("a");
    expect(now(store).actionError).toBeNull();
    // Its list read again: the start shows.
    await vi.waitFor(() => expect(backend.snapshot).toHaveBeenCalledTimes(2));
    store.close();
  });

  it("clears 'not current' once a poll works, and drops a poll a load overtook", async () => {
    vi.useFakeTimers();
    try {
      const { backend } = fakeBackend([container("a", "running")]);
      const store = createStore(backend);
      await store.ready;
      await store.open();
      store.watchLinks(true);
      backend.links = vi.fn(async () => {
        throw { kind: "timeout", message: "slow" };
      });
      await vi.advanceTimersByTimeAsync(LINKS_MS);
      expect(now(store).stale?.kind).toBe("timeout");
      const fresh = { ...snap([]).links, version: 1 };
      backend.links = vi.fn(async () => fresh);
      await vi.advanceTimersByTimeAsync(LINKS_MS);
      expect(now(store).stale).toBeNull();
      expect(now(store).links).toBe(fresh);
      // A poll answered after a load began: the load's links stand.
      let answer: (links: Links) => void = () => {};
      backend.links = vi.fn(() => new Promise<Links>((resolve) => (answer = resolve)));
      await vi.advanceTimersByTimeAsync(LINKS_MS);
      await store.open();
      const loaded = now(store).links;
      answer({ ...fresh });
      await vi.advanceTimersByTimeAsync(0);
      expect(now(store).links).toBe(loaded);
      store.close();
    } finally {
      vi.useRealTimers();
    }
  });

  it("says a settings change that failed, and reads the settings again", async () => {
    const { backend } = fakeBackend([]);
    backend.setStartVmOnOpen = vi.fn(async () => {
      throw { kind: "settings", message: "the settings are a newer app's" };
    });
    const store = createStore(backend);
    await store.ready;
    await store.setStartVmOnOpen(false);
    expect(now(store).settingsError?.message).toBe("the settings are a newer app's");
    expect(backend.status).toHaveBeenCalledTimes(1);
    store.close();
  });
});

/** The first wait after a stream that ended at once. */
const BACKOFF = 1000;

describe("the inspector's focus", () => {
  const isolated = (cgroup: string) => ({
    version: 1,
    name: "a",
    namespaces: [],
    cgroup,
    init_pid: 1,
    command: null,
    user: null,
  });
  const focusOf = (store: ReturnType<typeof createStore>) => {
    let value: Focus | null = null;
    store.focus.subscribe((focus) => (value = focus))();
    return value as Focus | null;
  };

  it("follows the selection's stats, keeps its last samples, and lets the last one go", async () => {
    const { backend, handlers } = fakeBackend([
      { ...container("a", "running"), Image: "app:1", ImageID: "sha256:aa" },
      container("b", "exited"),
    ]);
    backend.isolation = vi.fn(async () => isolated("/x"));
    const store = createStore(backend);
    await store.ready;
    await store.open();
    await store.focusOn("a");
    expect(backend.watchStats).toHaveBeenCalledWith("a", expect.any(Number));
    expect(focusOf(store)?.isolation?.cgroup).toBe("/x");
    for (let i = 0; i < SAMPLES + 5; i++) handlers.stats?.({ id: "a", cpu: i, mem: 1, limit: 2 });
    // Another container's sample (a stream ending late) is no one's.
    handlers.stats?.({ id: "b", cpu: 999, mem: 1, limit: 2 });
    const samples = focusOf(store)?.samples ?? [];
    expect(samples).toHaveLength(SAMPLES);
    expect(samples.at(-1)?.cpu).toBe(SAMPLES + 4);
    // Its stream's own end is said.
    handlers.statsEnded?.({ id: "a", why: "the stream ended" });
    expect(focusOf(store)?.ended).toBe("the stream ended");
    // A stopped one: no isolation asked, no stats followed, the last let go.
    for (const asked of [backend.isolation, backend.watchStats, backend.unwatchStats]) {
      (asked as ReturnType<typeof vi.fn>).mockClear();
    }
    await store.focusOn("b");
    expect(backend.unwatchStats).toHaveBeenCalled();
    expect(backend.isolation).not.toHaveBeenCalled();
    expect(backend.watchStats).not.toHaveBeenCalled();
    expect(focusOf(store)?.samples).toEqual([]);
    await store.focusOn(null);
    expect(focusOf(store)).toBeNull();
    store.close();
  });

  it("never follows a selection a later one overtook", async () => {
    const { backend } = fakeBackend([container("a", "running"), container("b", "running")]);
    let answerA = () => {};
    backend.isolation = vi.fn(async (id: string) => {
      if (id === "a") await new Promise<void>((resolve) => (answerA = resolve));
      return isolated(`/${id}`);
    });
    const store = createStore(backend);
    await store.ready;
    await store.open();
    const first = store.focusOn("a");
    await store.focusOn("b");
    answerA();
    await first;
    // b's alone, as each ask says which selection it's for.
    expect(backend.watchStats).toHaveBeenCalledTimes(1);
    expect(backend.watchStats).toHaveBeenCalledWith("b", 2);
    expect(backend.unwatchStats).toHaveBeenLastCalledWith(2);
    expect(focusOf(store)?.isolation?.cgroup).toBe("/b");
    store.close();
  });

  it("follows the selection again on a new connection", async () => {
    vi.useFakeTimers();
    try {
      const { backend, handlers } = fakeBackend([container("a", "running")]);
      backend.isolation = vi.fn(async () => isolated("/x"));
      const store = createStore(backend);
      await store.ready;
      await store.open();
      await store.focusOn("a");
      expect(backend.watchStats).toHaveBeenCalledTimes(1);
      // The stream lost (the stats' with it): followed again once back.
      handlers.lost?.("ended");
      await vi.advanceTimersByTimeAsync(BACKOFF);
      expect(backend.follow).toHaveBeenCalledTimes(2);
      expect(backend.watchStats).toHaveBeenCalledTimes(2);
      expect(backend.isolation).toHaveBeenCalledTimes(2);
      store.close();
    } finally {
      vi.useRealTimers();
    }
  });
});

describe("the actions", () => {
  it("does one action at a time a container, says a failure and forgets it after", async () => {
    const { backend } = fakeBackend([container("a", "running"), container("b", "running")]);
    let finish = () => {};
    backend.stopContainer = vi.fn(() => new Promise<void>((resolve) => (finish = resolve)));
    const store = createStore(backend);
    await store.ready;
    await store.open();
    const stopping = store.act("a", "stop");
    expect(now(store).busy.has("a")).toBe(true);
    // A second click while it runs does nothing; another container's goes.
    expect(await store.act("a", "stop")).toBe(false);
    expect(await store.act("b", "pause")).toBe(true);
    expect(backend.stopContainer).toHaveBeenCalledTimes(1);
    finish();
    expect(await stopping).toBe(true);
    expect(now(store).busy.size).toBe(0);
    // A refusal is said, the busy mark let go; the next that works clears it.
    backend.restartContainer = vi.fn(async () => {
      throw { kind: "api", message: "no" };
    });
    expect(await store.act("a", "restart")).toBe(false);
    expect(now(store).actionError?.message).toBe("no");
    expect(now(store).busy.size).toBe(0);
    // A load in between (a resync, an event) keeps it said.
    await store.open();
    expect(now(store).actionError?.message).toBe("no");
    expect(await store.act("a", "remove", true)).toBe(true);
    expect(backend.removeContainer).toHaveBeenCalledWith("a", true);
    expect(now(store).actionError).toBeNull();
    store.close();
  });

  it("runs from the sheet, saying the pull's lines while it goes and its failure after", async () => {
    const { backend, handlers } = fakeBackend([container("a", "running")]);
    let finish = (_id: string) => {};
    backend.runImage = vi.fn(() => new Promise<string>((resolve) => (finish = resolve)));
    const store = createStore(backend);
    await store.ready;
    await store.open();
    const spec = { image: "alpine:3.20", name: null, memoryMib: 256, port: null };
    const running = store.run(spec);
    handlers.runLine?.("a1b2 Downloading 25%");
    expect(now(store).run).toEqual({ image: "alpine:3.20", going: true, line: "a1b2 Downloading 25%", error: null });
    expect(await store.run(spec)).toBeNull();
    finish("new");
    expect(await running).toBe("new");
    expect(now(store).run).toBeNull();
    // A line after the run is no one's.
    handlers.runLine?.("late");
    expect(now(store).run).toBeNull();
    backend.runImage = vi.fn(async () => {
      throw { kind: "api", message: "the name is in use" };
    });
    expect(await store.run(spec)).toBeNull();
    expect(now(store).run).toEqual({
      image: "alpine:3.20",
      going: false,
      line: null,
      error: { kind: "api", message: "the name is in use" },
    });
    store.clearRun();
    expect(now(store).run).toBeNull();
    store.close();
  });

  it("reads the list again after a container's end: a restart policy's is restarting", async () => {
    const { backend, handlers } = fakeBackend([container("a", "running")]);
    const store = createStore(backend);
    await store.ready;
    await store.open();
    backend.snapshot = vi.fn(async () => snap([container("a", "restarting")]));
    handlers.event?.({ Type: "container", Action: "die", Actor: { ID: "a" } });
    await vi.waitFor(() => expect(now(store).containers.get("a")?.State).toBe("restarting"));
    store.close();
  });

  it("follows the selection's logs, keeps their last lines, and lets the last one go", async () => {
    const { backend, handlers } = fakeBackend([container("a", "running"), container("b", "exited")]);
    const store = createStore(backend);
    await store.ready;
    await store.open();
    const logsOf = () => {
      let value: Logs | null = null;
      store.logs.subscribe((logs) => (value = logs))();
      return value as Logs | null;
    };
    await store.focusOn("a");
    expect(backend.logsFollow).toHaveBeenLastCalledWith("a", true, expect.any(Number), expect.any(Function));
    const fromA = handlers.logBatch!;
    fromA({ lines: Array.from({ length: LOG_LINES + 3 }, (_, i) => `line ${i}`), ended: null });
    expect(logsOf()?.lines).toHaveLength(LOG_LINES);
    expect(logsOf()?.lines.at(-1)).toEqual({ n: LOG_LINES + 2, text: `line ${LOG_LINES + 2}` });
    // Its end, said.
    fromA({ lines: [], ended: "the API went" });
    expect(logsOf()?.lines.at(-1)?.text).toBe("[the logs ended: the API went]");
    // Followed again (it restarted): a's earlier follower's lines dropped.
    const before = logsOf()?.lines.length;
    await store.focusOn("a");
    fromA({ lines: ["stale"], ended: null });
    expect(logsOf()?.lines).toEqual([]);
    expect(before).toBeGreaterThan(0);
    // Another: its own logs, not followed (it's stopped), never stopped by
    // a stop of its own (its follow stops the last itself).
    (backend.logsStop as ReturnType<typeof vi.fn>).mockClear();
    await store.focusOn("b");
    expect(backend.logsStop).not.toHaveBeenCalled();
    expect(backend.logsFollow).toHaveBeenLastCalledWith("b", false, expect.any(Number), expect.any(Function));
    fromA({ lines: ["late"], ended: null });
    expect(logsOf()).toEqual({ id: "b", lines: [] });
    await store.focusOn(null);
    expect(backend.logsStop).toHaveBeenCalled();
    expect(logsOf()).toBeNull();
    store.close();
  });

  it("reads the images once followed, again on an image's event, and after a removal", async () => {
    const { backend, handlers } = fakeBackend([container("a", "running")]);
    const one = { id: "sha256:a", tags: ["a:1"], created: 0, layers: [] };
    backend.images = vi.fn(async () => [one]);
    const store = createStore(backend);
    await store.ready;
    await store.open();
    await vi.waitFor(() => expect(now(store).images).toEqual([one]));
    expect(backend.images).toHaveBeenCalledTimes(1);
    handlers.event?.({ Type: "image", Action: "pull", Actor: { ID: "sha256:b" } });
    await vi.waitFor(() => expect(backend.images).toHaveBeenCalledTimes(2));
    // A refusal said, the images read again all the same.
    backend.removeImage = vi.fn(async () => {
      throw { kind: "api", message: "in use" };
    });
    expect(await store.removeImages([{ id: "sha256:a", tags: ["a:1"] }])).toBe(false);
    expect(now(store).actionError?.message).toBe("in use");
    await vi.waitFor(() => expect(backend.images).toHaveBeenCalledTimes(3));
    // Several: one after another, each by its tags (an untagged one by
    // its ID), never forced.
    backend.removeImage = vi.fn(async () => {});
    expect(await store.removeImages([{ id: "sha256:a", tags: ["a:1", "b:2"] }, { id: "sha256:c", tags: [] }])).toBe(true);
    expect((backend.removeImage as ReturnType<typeof vi.fn>).mock.calls).toEqual([
      ["a:1", false],
      ["b:2", false],
      ["sha256:c", false],
    ]);
    store.close();
  });

  it("says a section's failed read until that reader reads again, never another's", async () => {
    const { backend, handlers } = fakeBackend([container("a", "running")]);
    let fail = true;
    backend.volumes = vi.fn(async () => {
      if (fail) throw { kind: "api", message: "volumes refused" };
      return { version: 1, volumes: [], warnings: [] };
    });
    const store = createStore(backend);
    await store.ready;
    await store.open();
    await vi.waitFor(() => expect(now(store).stale?.message).toBe("volumes refused"));
    // Another reader's success leaves it said.
    handlers.event?.({ Type: "image", Action: "pull", Actor: { ID: "i" } });
    await vi.waitFor(() => expect(backend.images).toHaveBeenCalledTimes(2));
    expect(now(store).stale?.message).toBe("volumes refused");
    // Its own reads again: cleared.
    fail = false;
    await store.readVolumes();
    expect(now(store).stale).toBeNull();
    store.close();
  });

  it("reads the volumes once followed and on their events, and deletes them by name in turn", async () => {
    const { backend, handlers } = fakeBackend([container("a", "running")]);
    const volumes = { version: 1, volumes: [], warnings: [] };
    backend.volumes = vi.fn(async () => volumes);
    const store = createStore(backend);
    await store.ready;
    await store.open();
    await vi.waitFor(() => expect(now(store).volumes).toEqual(volumes));
    // A volume made: the volumes and the list (the rail's counts) read again.
    const lists = (backend.snapshot as ReturnType<typeof vi.fn>).mock.calls.length;
    handlers.event?.({ Type: "volume", Action: "create", Actor: { ID: "v" } });
    await vi.waitFor(() => expect(backend.volumes).toHaveBeenCalledTimes(2));
    await vi.waitFor(() => expect((backend.snapshot as ReturnType<typeof vi.fn>).mock.calls.length).toBeGreaterThan(lists));
    // Deleted one after another, by name; a refusal said, the volumes read again.
    backend.removeVolume = vi.fn(async (name: string) => {
      if (name === "used") throw { kind: "api", message: "in use" };
    });
    expect(await store.removeVolumes(["spare", "used", "never"])).toBe(false);
    expect((backend.removeVolume as ReturnType<typeof vi.fn>).mock.calls).toEqual([["spare"], ["used"]]);
    expect(now(store).actionError?.message).toBe("in use");
    await vi.waitFor(() => expect(backend.volumes).toHaveBeenCalledTimes(3));
    store.close();
  });

  it("reads the bridges' own views only while they're shown, keeping one that fails, and the list again on a network's event", async () => {
    const { backend, handlers } = fakeBackend([container("a", "running")]);
    const bridge = (name: string, driver = "bridge") => ({ name, short: name, subnet: null, gateway: null, internal: false, driver });
    backend.snapshot = vi.fn(async () => ({ ...snap([container("a", "running")]), networks: [bridge("front"), bridge("host", "host"), bridge("gone")] }));
    const front = { name: "front", bridge: "sy-br1", gateway: "10.1.0.1", internal: false, dns_running: true, endpoints: [] };
    let frontFails = false;
    backend.networkView = vi.fn(async (name: string) => {
      if (name === "gone" || frontFails) throw { kind: "api", message: `no ${name}` };
      return front;
    });
    const views = () => (backend.networkView as ReturnType<typeof vi.fn>).mock.calls;
    const store = createStore(backend);
    await store.ready;
    await store.open();
    await vi.waitFor(() => expect(now(store).phase).toBe("live"));
    expect(views()).toEqual([]);
    // Shown: the bridges alone; one that fails is said, the rest kept.
    store.watchNetworks(true);
    await vi.waitFor(() => expect([...now(store).networkViews]).toEqual([["front", front]]));
    expect(views()).toEqual([["front"], ["gone"]]);
    expect(now(store).stale?.message).toBe("no gone");
    // A start (a new veth): read again; front failing now keeps what it said.
    frontFails = true;
    handlers.event?.({ Type: "container", Action: "start", Actor: { ID: "a" } });
    await vi.waitFor(() => expect(views()).toHaveLength(4));
    await vi.waitFor(() => expect(now(store).stale?.message).toBe("no front"));
    expect(now(store).networkViews.get("front")).toEqual(front);
    // Hidden: a start reads none (a volume's event, read after it, the control).
    store.watchNetworks(false);
    handlers.event?.({ Type: "container", Action: "start", Actor: { ID: "a" } });
    handlers.event?.({ Type: "volume", Action: "create", Actor: { ID: "v" } });
    await vi.waitFor(() => expect(backend.volumes).toHaveBeenCalledTimes(2));
    expect(views()).toHaveLength(4);
    // A network made: the list read again.
    const lists = (backend.snapshot as ReturnType<typeof vi.fn>).mock.calls.length;
    handlers.event?.({ Type: "network", Action: "create", Actor: { ID: "n" } });
    await vi.waitFor(() => expect((backend.snapshot as ReturnType<typeof vi.fn>).mock.calls.length).toBeGreaterThan(lists));
    store.close();
  });
});
