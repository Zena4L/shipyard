// What the page asks of the app's core (step 14.2): only these commands,
// each allowed by name in the core's capability, and the events it
// listens to. Tests give the store a backend of their own.
import { Channel, invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

/** Where the Mac's VM stands (the host's phase; never a connection to it). */
export type VmState =
  | { state: "stopped" }
  | { state: "running"; phase: string; serving: boolean };

/** A command's failure, as the core says it (`shipyard-client`'s kinds). */
export interface Failure {
  kind:
    | "unreachable"
    | "newer"
    | "older"
    | "api"
    | "timeout"
    | "vm-start"
    | "shipyard"
    | "settings"
    | "ended"
    | "other";
  message: string;
}

/** What the core knows before anything is loaded. */
export interface Status {
  /** `macos` or `linux`. */
  platform: string;
  /** The Mac's VM (none on Linux). */
  vm: VmState | null;
  /** Start the VM when the app opens (the setting). */
  startVmOnOpen: boolean;
  /** The `shipyard` the core runs, or why there's none. */
  shipyard: { path: string } | { missing: string };
  /** What was wrong with the settings file, if anything. */
  settingsSaid: string | null;
}

/** A container, as the API lists it (Docker's shape). */
export interface Container {
  Id: string;
  Names?: string[];
  Image?: string;
  /** Its image's ID (the image it runs, whatever its name now names). */
  ImageID?: string;
  State?: string;
  Status?: string;
  Labels?: Record<string, string>;
  Ports?: { PrivatePort?: number; PublicPort?: number; Type?: string }[];
  /** Its networks, by name, with its address on each. */
  NetworkSettings?: { Networks?: Record<string, { IPAddress?: string }> };
  /** What's mounted in it: volumes (by name) and binds. */
  Mounts?: { Type?: string; Name?: string; Source?: string; Destination?: string; RW?: boolean }[];
}

/** A network, as the core lists it (`shipyard-client`'s `Network`). */
export interface Network {
  name: string;
  /** Compose's name for it in its project, else its name. */
  short: string;
  subnet: string | null;
  /** That subnet's gateway. */
  gateway: string | null;
  internal: boolean;
  /** `bridge`, or a built-in's (`host`, `null`, `pasta`). */
  driver: string;
}

/** What Docker's `info` says, what the rail and status bar need of it. */
export interface Info {
  KernelVersion?: string;
  MemTotal?: number;
  Architecture?: string;
  ServerVersion?: string;
  CgroupVersion?: string;
  Driver?: string;
  SecurityOptions?: string[];
}

/** A link of the links answer: declared (compose's `depends_on`) or
 * observed (connections seen between two containers). */
export interface LinkEdge {
  from: string;
  /** A container's ID, or a missing service's (`project/service`). */
  to: string;
  to_missing: boolean;
  kind: "declared" | "observed";
  port: number | null;
  protocol: string | null;
  condition: string | null;
  connections: number;
  via_host_port: boolean;
  refused: boolean;
}

/** A compose service some container depends on, with no container. */
export interface Missing {
  id: string;
  project: string;
  service: string;
}

/** A port a container publishes on the host. */
export interface Entry {
  container: string;
  host_ip: string;
  host_port: number;
  container_port: number;
  protocol: string;
  /** Its connections from outside the containers (absent before 14.3). */
  connections?: number;
}

/** Shipyard's links answer (`/shipyard/links`, version 1). */
export interface Links {
  version: number;
  containers: unknown[];
  missing?: Missing[];
  entries?: Entry[];
  links: LinkEdge[];
  observed: { available: boolean; cause: string | null; reason: string | null };
}

/** Everything loaded at once. */
export interface Snapshot {
  containers: Container[];
  links: Links;
  info: Info;
  /** How many of each the other sections hold. */
  counts: { images: number; volumes: number; networks: number };
  /** Every network, in the API's order. */
  networks: Network[];
}

/** What the host and each running container and its monitor use now
 * (`/shipyard/usage`): counters, the page divides two reads. Bytes and
 * nanoseconds. */
export interface Usage {
  /** The host's (on a Mac, its VM's). */
  memory: { total: number; available: number };
  /** The host's CPU time since boot, in all its CPUs. */
  cpu: { time: number; cpus: number };
  /** Each running or paused container whose cgroup could be read. */
  containers: { id: string; cpu_time: number; memory: number; monitor_memory: number | null }[];
}

/** The namespace kinds a container's isolation says, in its order. */
export const NAMESPACE_KINDS = ["cgroup", "ipc", "mnt", "net", "pid", "user", "uts", "time"] as const;

/** A running container's isolation (`/shipyard/containers/{id}/isolation`,
 * version 1). */
export interface Isolation {
  version: number;
  name: string;
  namespaces: { kind: string; own: boolean; present: boolean; id: number | null }[];
  cgroup: string | null;
  init_pid: number;
  command: { pid: number; args: string[]; truncated: boolean } | null;
  user: { inside: number; outside: number; count: number } | null;
}

/** An image with its layers, bottom to top (the client's `ImageLayers`). */
export interface StoredImage {
  id: string;
  tags: string[];
  /** Seconds since the epoch (0: unknown). */
  created: number;
  layers: { diff: string; size: number; created_by: string }[];
}

/** An entry at a volume's top level (14.1b's answer). */
export interface VolumeFile {
  name: string;
  kind: "file" | "directory" | "link" | "other";
  size: number;
}

/** A volume (`/shipyard/volumes`' entry, with Docker's `Mountpoint`): one
 * Docker's list has that the shared walk hasn't reached yet has no files,
 * size 0, not `complete`. */
export interface StoredVolume {
  name: string;
  /** RFC 3339. */
  created: string;
  /** Bytes; at least this when not `complete`. */
  size: number;
  complete: boolean;
  files: VolumeFile[];
  /** How many more entries its top level has. */
  more: number;
  /** Where its data is. */
  mountpoint: string;
}

/** Every volume (`/shipyard/volumes`, version 1). */
export interface VolumesAnswer {
  version: number;
  volumes: StoredVolume[];
  warnings: string[];
}

/** Shipyard's own view of a bridge network (the client's `NetworkView`,
 * read from `/shipyard/networks/{name}`). */
export interface NetworkView {
  name: string;
  bridge: string;
  gateway: string;
  internal: boolean;
  dns_running: boolean;
  /** The containers on it: the host's end of each one's veth pair. */
  endpoints: { container: string | null; address: string; host_veth: string }[];
}

/** Lines of a container's logs, and, at its follower's end, why. */
export interface LogBatch {
  lines: string[];
  ended: string | null;
}

/** What the Run sheet asks for (the client's `RunSpec`): the core builds
 * the container from these alone. */
export interface RunSpec {
  image: string;
  name: string | null;
  /** MiB: one of `MEMORY_CHOICES`. */
  memoryMib: number;
  /** `HOST:CONTAINER` or `CONTAINER`. */
  port: string | null;
}

/** The memory limits the Run sheet offers, MiB (the mock's). */
export const MEMORY_CHOICES = [128, 256, 512, 1024, 2048] as const;

/** A stats stream that ended on its own. */
export interface StatsEnded {
  id: string;
  why: string;
}

/** One stats sample of a container (the client's `Sample`). */
export interface Sample {
  id: string;
  /** CPU, percent of one CPU. */
  cpu: number;
  /** Memory in use, bytes. */
  mem: number;
  /** Its limit, bytes. */
  limit: number;
}

/** What the inspector reads of Docker's inspect. */
export interface Inspected {
  State?: { OOMKilled?: boolean; ExitCode?: number };
}

/** An event of the API's (Docker's shape). */
export interface EngineEvent {
  Type?: string;
  Action?: string;
  Actor?: { ID?: string; Attributes?: Record<string, string> };
}

/** The core's commands and events. */
export interface Backend {
  status(): Promise<Status>;
  startVm(): Promise<void>;
  snapshot(): Promise<Snapshot>;
  /** The links answer alone (the map's poll). */
  links(): Promise<Links>;
  /** What everything uses now (the live numbers' poll). */
  usage(): Promise<Usage>;
  /** Starts a container, by its ID. */
  startContainer(id: string): Promise<void>;
  /** Stops a container. */
  stopContainer(id: string): Promise<void>;
  /** Restarts a container. */
  restartContainer(id: string): Promise<void>;
  /** Pauses a container. */
  pauseContainer(id: string): Promise<void>;
  /** Resumes a paused container. */
  unpauseContainer(id: string): Promise<void>;
  /** Removes a container (`force`: a running one stopped first). */
  removeContainer(id: string, force: boolean): Promise<void>;
  /** Runs an image detached from the Run sheet's spec: the new
   * container's ID. */
  runImage(spec: RunSpec): Promise<string>;
  /** Each line of a run's pull. */
  onRunLine(handler: (line: string) => void): Promise<UnlistenFn>;
  /** Every image, with its layers. */
  images(): Promise<StoredImage[]>;
  /** Removes an image's tag, or an untagged image by its ID (`force`: past
   * the engine's in-use check: the page never asks it). */
  removeImage(reference: string, force: boolean): Promise<void>;
  /** Every volume, with its size, files and place. */
  volumes(): Promise<VolumesAnswer>;
  /** Deletes a volume by name, never forced. */
  removeVolume(name: string): Promise<void>;
  /** Shipyard's own view of the bridge network `name`; null where bridges
   * aren't served (rootless). */
  networkView(name: string): Promise<NetworkView | null>;
  /** Follows a container's logs for the page's `selection` (its last
   * lines, then each new one with `follow`), in batches to `onbatch`. */
  logsFollow(id: string, follow: boolean, selection: number, onbatch: (batch: LogBatch) => void): Promise<void>;
  /** Stops following logs, for the page's `selection`. */
  logsStop(selection: number): Promise<void>;
  /** Opens a shell in a running container on a terminal of `cols` ×
   * `rows`, its bytes to `onoutput` (empty bytes: its end, after its last
   * output): its session's number. */
  shellOpen(id: string, cols: number, rows: number, onoutput: (bytes: Uint8Array) => void): Promise<number>;
  /** Says `bytes` more of a shell's output were written: the core sends
   * more once enough is. */
  shellAck(session: number, bytes: number): Promise<void>;
  /** Types into a shell. */
  shellInput(session: number, data: string): Promise<void>;
  /** Resizes a shell's terminal. */
  shellResize(session: number, cols: number, rows: number): Promise<void>;
  /** Closes a shell. */
  shellClose(session: number): Promise<void>;
  /** A running container's isolation. */
  isolation(id: string): Promise<Isolation>;
  /** A container's Docker inspect. */
  inspect(id: string): Promise<Inspected>;
  /** Follows a container's stats for the page's `selection` (the one
   * followed before ends; an ask for an older selection than the core's
   * latest changes nothing). */
  watchStats(id: string, selection: number): Promise<void>;
  /** Stops following stats, for the page's `selection`. */
  unwatchStats(selection: number): Promise<void>;
  follow(): Promise<void>;
  release(): Promise<void>;
  setStartVmOnOpen(start: boolean): Promise<void>;
  chooseShipyard(): Promise<void>;
  onEvent(handler: (event: EngineEvent) => void): Promise<UnlistenFn>;
  onVmLine(handler: (line: string) => void): Promise<UnlistenFn>;
  onLost(handler: (why: string) => void): Promise<UnlistenFn>;
  onStats(handler: (sample: Sample) => void): Promise<UnlistenFn>;
  /** A stats stream's end on its own (the container stopped, the API
   * refused): its container and why. */
  onStatsEnded(handler: (ended: StatsEnded) => void): Promise<UnlistenFn>;
}

/** The app's own core, through Tauri's bridge. */
export const tauriBackend: Backend = {
  status: () => invoke<Status>("status"),
  startVm: () => invoke("start_vm"),
  snapshot: () => invoke<Snapshot>("snapshot"),
  links: () => invoke<Links>("links"),
  usage: () => invoke<Usage>("usage"),
  startContainer: (id) => invoke("start_container", { id }),
  stopContainer: (id) => invoke("stop_container", { id }),
  restartContainer: (id) => invoke("restart_container", { id }),
  pauseContainer: (id) => invoke("pause_container", { id }),
  unpauseContainer: (id) => invoke("unpause_container", { id }),
  removeContainer: (id, force) => invoke("remove_container", { id, force }),
  runImage: (spec) => invoke("run_image", { spec }),
  onRunLine: (handler) => listen<string>("run-line", (event) => handler(event.payload)),
  images: () => invoke("images"),
  removeImage: (reference, force) => invoke("remove_image", { reference, force }),
  volumes: () => invoke("volumes"),
  removeVolume: (name) => invoke("remove_volume", { name }),
  networkView: (name) => invoke("network_view", { name }),
  logsFollow: (id, follow, selection, onbatch) => {
    const lines = new Channel<LogBatch>();
    lines.onmessage = onbatch;
    return invoke("logs_follow", { id, follow, selection, lines });
  },
  logsStop: (selection) => invoke("logs_stop", { selection }),
  shellOpen: (id, cols, rows, onoutput) => {
    const output = new Channel<ArrayBuffer>();
    output.onmessage = (bytes) => onoutput(new Uint8Array(bytes));
    return invoke("shell_open", { id, cols, rows, output });
  },
  shellAck: (session, bytes) => invoke("shell_ack", { session, bytes }),
  shellInput: (session, data) => invoke("shell_input", { session, data }),
  shellResize: (session, cols, rows) => invoke("shell_resize", { session, cols, rows }),
  shellClose: (session) => invoke("shell_close", { session }),
  isolation: (id) => invoke<Isolation>("isolation", { id }),
  inspect: (id) => invoke<Inspected>("inspect", { id }),
  watchStats: (id, selection) => invoke("watch_stats", { id, selection }),
  unwatchStats: (selection) => invoke("unwatch_stats", { selection }),
  follow: () => invoke("follow"),
  release: () => invoke("release"),
  setStartVmOnOpen: (start) => invoke("set_start_vm_on_open", { start }),
  chooseShipyard: () => invoke("choose_shipyard"),
  onEvent: (handler) => listen<EngineEvent>("engine-event", (event) => handler(event.payload)),
  onVmLine: (handler) => listen<string>("vm-line", (event) => handler(event.payload)),
  onLost: (handler) => listen<string>("connection-lost", (event) => handler(event.payload)),
  onStats: (handler) => listen<Sample>("stats", (event) => handler(event.payload)),
  onStatsEnded: (handler) => listen<StatsEnded>("stats-ended", (event) => handler(event.payload)),
};
