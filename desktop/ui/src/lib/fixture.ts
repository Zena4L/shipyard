// The mock's own data (docs/design/shipyard-ui.html), as the API would give
// it: what the visual test feeds the app, so the app and the mock can be
// compared pixel for pixel. Never in the app's build (preview.html alone).
import { NAMESPACE_KINDS, type Backend, type Container, type Entry, type Isolation, type LinkEdge, type Network, type NetworkView, type Sample, type Snapshot, type StoredImage, type StoredVolume, type Usage } from "./backend";
import { SAMPLES } from "./store";

/** The time the fixture is seen at. */
export const NOW = Date.parse("2026-10-04T12:00:00Z");

/** ISO 6346's check digit, as the mock computes it. */
function checkDigit(code: string): number {
  const values: Record<string, number> = {};
  let value = 10;
  for (const letter of "ABCDEFGHIJKLMNOPQRSTUVWXYZ") {
    if (value % 11 === 0) value++;
    values[letter] = value++;
  }
  let sum = 0;
  for (let i = 0; i < 10; i++) {
    const c = code[i] ?? "0";
    sum += (/\d/.test(c) ? Number(c) : (values[c] ?? 0)) * 2 ** i;
  }
  return (sum % 11) % 10;
}

const MIB = 1024 * 1024;
const minutes = (n: number) => new Date(NOW - n * 60_000).toISOString();

interface Spec {
  serial: string;
  name: string;
  project: string | null;
  image: string;
  state: string;
  exit: number | null;
  memMax: number;
  mem: number;
  cpu: number;
  ports: [number, number][];
  startedMinutesAgo: number | null;
  status: string;
}

// The mock's containers, in its order; CPU as its series give it when
// Math.random is 0.5 (the test fixes it so): each series' base.
const SPECS: Spec[] = [
  { serial: "304812", name: "web", project: "acme-shop", image: "nginx:1.27-alpine", state: "running", exit: null, memMax: 256, mem: 18.4, cpu: 2, ports: [[8080, 80]], startedMinutesAgo: 192, status: "Up 3 hours" },
  { serial: "304813", name: "api", project: "acme-shop", image: "ghcr.io/acme/api:2.4.1", state: "running", exit: null, memMax: 1024, mem: 212, cpu: 9, ports: [[3000, 3000]], startedMinutesAgo: 192, status: "Up 3 hours" },
  { serial: "304814", name: "postgres", project: "acme-shop", image: "postgres:16.4", state: "running", exit: null, memMax: 2048, mem: 486, cpu: 4, ports: [[5432, 5432]], startedMinutesAgo: 193, status: "Up 3 hours" },
  { serial: "304815", name: "redis", project: "acme-shop", image: "redis:7.4-alpine", state: "running", exit: null, memMax: 256, mem: 9.1, cpu: 0.6, ports: [], startedMinutesAgo: 193, status: "Up 3 hours" },
  { serial: "304816", name: "worker", project: "acme-shop", image: "ghcr.io/acme/worker:2.4.1", state: "exited", exit: 137, memMax: 256, mem: 256, cpu: 0, ports: [], startedMinutesAgo: null, status: "Exited (137) 14 minutes ago" },
  { serial: "209441", name: "build-cache", project: null, image: "rust:1.98-slim", state: "exited", exit: 0, memMax: 4096, mem: 0, cpu: 0, ports: [], startedMinutesAgo: null, status: "Exited (0) 2 hours ago" },
  { serial: "118027", name: "playground", project: null, image: "alpine:3.20", state: "paused", exit: null, memMax: 128, mem: 1.2, cpu: 0, ports: [], startedMinutesAgo: 6, status: "Up 6 minutes (Paused)" },
];

const idOf = (spec: Spec | string) => (typeof spec === "string" ? spec : spec.name).padEnd(64, "0");

// The mock's networks, in its order, as the API names them (a compose
// project's by its project), and its default network as Shipyard's.
const NETWORKS: Network[] = [
  { name: "acme-shop_frontend", short: "frontend", subnet: "10.89.1.0/24", gateway: "10.89.1.1", internal: false, driver: "bridge" },
  { name: "acme-shop_backend", short: "backend", subnet: "10.89.2.0/24", gateway: "10.89.2.1", internal: true, driver: "bridge" },
  // Shipyard's default network, keyed as the mock keys it.
  { name: "bridge", short: "shipyard", subnet: "10.88.0.0/16", gateway: "10.88.0.1", internal: false, driver: "bridge" },
];

// The mock's bridges, by network.
const BRIDGES: Record<string, string> = { "acme-shop_frontend": "sy-br7c1e", "acme-shop_backend": "sy-br2f90", bridge: "sy-br0" };

// Each container's networks and mounts, as the mock attaches them.
const ATTACH: Record<string, { nets: Record<string, string>; mounts: [string, string, boolean][] }> = {
  web: { nets: { "acme-shop_frontend": "10.89.1.2" }, mounts: [["uploads", "/usr/share/nginx/html/uploads", false]] },
  api: { nets: { "acme-shop_frontend": "10.89.1.3", "acme-shop_backend": "10.89.2.2" }, mounts: [["uploads", "/srv/uploads", true]] },
  postgres: { nets: { "acme-shop_backend": "10.89.2.3" }, mounts: [["pgdata", "/var/lib/postgresql/data", true]] },
  redis: { nets: { "acme-shop_backend": "10.89.2.4" }, mounts: [["redis-data", "/data", true]] },
  worker: { nets: { "acme-shop_backend": "10.89.2.5" }, mounts: [] },
  "build-cache": { nets: { bridge: "10.88.0.12" }, mounts: [["cargo-cache", "/usr/local/cargo/registry", true]] },
  playground: { nets: { bridge: "10.88.0.27" }, mounts: [] },
};

/** A link of the answer, `from` and `to` by name. */
const link = (from: string, to: string, kind: LinkEdge["kind"], port: number | null, connections: number): LinkEdge => ({
  from: idOf(from),
  to: idOf(to),
  to_missing: false,
  kind,
  port,
  protocol: port === null ? null : "tcp",
  condition: kind === "declared" ? "service_started" : null,
  connections,
  via_host_port: false,
  refused: false,
});

// The mock's links: its declared ones as compose declares them (no port),
// and the traffic it draws on each as observed connections (their port).
const LINKS: LinkEdge[] = [
  link("web", "api", "declared", null, 0),
  link("api", "postgres", "declared", null, 0),
  link("api", "redis", "declared", null, 0),
  link("worker", "redis", "declared", null, 0),
  link("web", "api", "observed", 3000, 4),
  link("api", "postgres", "observed", 5432, 3),
  link("api", "redis", "observed", 6379, 5),
  link("worker", "redis", "observed", 6379, 1),
  link("worker", "postgres", "observed", 5432, 2),
];

/** The snapshot the mock's data makes. */
export function snapshot(): Snapshot {
  const containers: Container[] = SPECS.map((spec) => ({
    Id: idOf(spec),
    Names: [`/${spec.name}`],
    Image: spec.image,
    State: spec.state,
    Status: spec.status,
    Ports: spec.ports.map(([PublicPort, PrivatePort]) => ({ PublicPort, PrivatePort, Type: "tcp" })),
    NetworkSettings: {
      Networks: Object.fromEntries(
        Object.entries(ATTACH[spec.name]?.nets ?? {}).map(([net, ip]) => [net, { IPAddress: ip }]),
      ),
    },
    Mounts: (ATTACH[spec.name]?.mounts ?? []).map(([name, path, rw]) => ({
      Type: "volume",
      Name: name,
      Destination: path,
      RW: rw,
    })),
  }));
  const entries: Entry[] = SPECS.filter((spec) => spec.state === "running").flatMap((spec) =>
    spec.ports.map(([host, container]) => ({
      container: idOf(spec),
      host_ip: "0.0.0.0",
      host_port: host,
      container_port: container,
      protocol: "tcp",
      connections: 2,
    })),
  );
  const nodes = SPECS.map((spec) => ({
    id: idOf(spec),
    name: spec.name,
    serial: `SHPU ${spec.serial} ${checkDigit(`SHPU${spec.serial}`)}`,
    memory_max: spec.memMax * MIB,
    started_at: spec.startedMinutesAgo === null ? null : minutes(spec.startedMinutesAgo),
    project: spec.project,
    service: spec.name,
    state: spec.state,
    health: null,
    exit_code: spec.exit,
    one_off: false,
    shares_network_with: [],
  }));
  return {
    containers,
    links: {
      version: 1,
      containers: nodes,
      missing: [],
      entries,
      links: LINKS,
      observed: { available: true, cause: null, reason: null },
    },
    info: {
      KernelVersion: "6.12.9",
      MemTotal: 4 * 1024 ** 3,
      Architecture: "aarch64",
      ServerVersion: "0.0.1",
      CgroupVersion: "2",
      Driver: "overlayfs",
      SecurityOptions: ["name=rootless"],
    },
    counts: { images: 9, volumes: 5, networks: 3 },
    networks: NETWORKS,
  };
}

/** The usage reads served so far: each read's counters a step on. */
let usageReads = 0;

/** The mock's live numbers as `/shipyard/usage`'s counters: four CPUs, a
 * second of the host's between two reads, each running or paused
 * container's CPU time its share of it; 1.1 GiB of 4 in use; 0.42 MiB a
 * monitor. */
export function usage(): Usage {
  const CPUS = 4;
  const SECOND = 1e9;
  usageReads += 1;
  const up = SPECS.filter((spec) => spec.state === "running" || spec.state === "paused");
  return {
    memory: { total: 4 * 1024 ** 3, available: (4 - 1.1) * 1024 ** 3 },
    cpu: { time: usageReads * CPUS * SECOND, cpus: CPUS },
    containers: up.map((spec) => ({
      id: idOf(spec),
      cpu_time: (usageReads * spec.cpu * SECOND) / 100,
      memory: spec.mem * MIB,
      monitor_memory: 0.42 * MIB,
    })),
  };
}

let statsHandler: ((sample: Sample) => void) | null = null;
let shellOutput: ((bytes: Uint8Array) => void) | null = null;
let typed = "";

// The mock's isolation of each running container: its seven namespaces
// (no time namespace), its cgroup by stack, its command and host PID.
const PROCESSES: Record<string, [string[], number]> = {
  web: [["nginx:", "master", "process"], 48213],
  api: [["/app/api", "--config", "/etc/api.toml"], 48240],
  postgres: [["postgres"], 48198],
  redis: [["redis-server", "*:6379"], 48201],
  playground: [["/bin/sh"], 50117],
};

/** The mock's isolation of container `id`. */
function isolationOf(id: string): Isolation {
  const spec = SPECS.find((s) => idOf(s) === id);
  const name = spec?.name ?? "";
  const [args, pid] = PROCESSES[name] ?? [[], 0];
  return {
    version: 1,
    name,
    namespaces: NAMESPACE_KINDS.map((kind, i) => ({
      kind,
      own: kind !== "time" || name === "playground",
      present: true,
      id: 4026531000 + i,
    })),
    cgroup: `/shipyard/${spec?.project ?? "standalone"}/${name}`,
    init_pid: pid - 1,
    command: { pid, args, truncated: false },
    user: { inside: 0, outside: 100000, count: 65536 },
  };
}

/** The mock's logs (`c.log`), as text: its markup gone, as a real log has none. */
const LOGS: Record<string, string[]> = {
  "web": [
    "/docker-entrypoint.sh: Configuration complete; ready for start up",
    "2026/10/04 09:01:12 [notice] 1#1: nginx/1.27.2",
    "2026/10/04 09:01:12 [notice] 1#1: start worker processes"
  ],
  "api": [
    "09:01:14.208 INFO  listening addr=0.0.0.0:3000",
    "09:01:14.211 INFO  db pool ready size=10"
  ],
  "postgres": [
    "2026-10-04 09:01:09 UTC LOG:  database system is ready to accept connections"
  ],
  "redis": [
    "1:M 04 Oct 2026 09:01:09.871 * Ready to accept connections tcp"
  ],
  "worker": [
    "11:57:02.114 INFO  processing batch=4181 items=2048",
    "11:57:09.880 INFO  processing batch=4182 items=2048",
    "11:57:11.302 WARN  memory pressure: 251 MiB / 256 MiB",
    "Killed",
    "[shipyard] oom-kill: memory.max reached in /shipyard/acme-shop/worker — exit 137"
  ],
  "build-cache": [
    "   Compiling shipyard-runtime v0.0.1",
    "   Compiling shipyard-cli v0.0.1",
    "    Finished `release` profile [optimized] target(s) in 41.07s"
  ],
  "playground": [
    "[shipyard] froze cgroup /shipyard/standalone/playground (cgroup.freeze=1)"
  ]
};

/** The mock's string hash (its layers' digests and its images' IDs). */
function hash(text: string): string {
  let x = 2166136261;
  for (const ch of text) {
    x ^= ch.charCodeAt(0);
    x = Math.imul(x, 16777619);
  }
  return (x >>> 0).toString(16).padStart(8, "0");
}

/** The mock's bases and images: layers bottom to top, `[command, MB]`, its
 * ages in days. A layer's digest is its command's hash, as the mock's. */
const BASES: Record<string, [string, number]> = {
  alpine: ["ADD alpine-minirootfs-3.20.3-aarch64.tar.gz /", 8.8],
  bookworm: ["ADD rootfs.tar.xz /   # debian:bookworm-slim", 97.1],
  distroless: ["FROM gcr.io/distroless/cc-debian12", 23.4],
};
const IMAGES: [string, number, string, [string, number][]][] = [
  ["nginx:1.27-alpine", 21, "alpine", [["RUN apk add --no-cache nginx=1.27.2-r0 nginx-module-njs", 38.6], ["COPY docker-entrypoint.sh /", 0.004], ["COPY 20-envsubst-on-templates.sh /docker-entrypoint.d/", 0.003]]],
  ["ghcr.io/acme/api:2.4.1", 2, "distroless", [["COPY --from=build /app/target/release/api /app/api", 18.2], ["COPY config/ /etc/api/", 0.01]]],
  ["postgres:16.4", 35, "bookworm", [["RUN apt-get install -y gosu locales libnss-wrapper", 21.4], ["RUN apt-get install -y postgresql-16=16.4-1.pgdg120+2", 312.6], ["COPY docker-entrypoint.sh /usr/local/bin/", 0.02]]],
  ["redis:7.4-alpine", 28, "alpine", [["RUN addgroup -S redis && adduser -S -G redis redis", 0.006], ["RUN apk add --no-cache redis=7.4.1-r0", 28.9], ["COPY docker-entrypoint.sh /usr/local/bin/", 0.002]]],
  ["ghcr.io/acme/worker:2.4.1", 2, "distroless", [["COPY --from=build /app/target/release/worker /app/worker", 14.7]]],
  ["rust:1.98-slim", 31, "bookworm", [["RUN apt-get install -y gcc libc6-dev ca-certificates", 186.0], ["RUN rustup-init -y --profile minimal --default-toolchain 1.98.1", 572.4]]],
  ["alpine:3.20", 60, "alpine", []],
  ["node:22-alpine", 90, "alpine", [["RUN apk add --no-cache libstdc++ && build node 22.9.0", 142.3], ["RUN npm i -g yarn@1.22.22", 5.4]]],
  ["python:3.13-slim", 90, "bookworm", [["RUN build python 3.13.0 --enable-optimizations", 41.8], ["RUN pip install --upgrade pip", 12.1]]],
];
let storedImages: StoredImage[] = IMAGES.map(([ref, days, base, own]) => ({
  id: `sha256:${hash(ref)}${hash(`${ref}!`)}${"0".repeat(48)}`,
  tags: [ref],
  created: Math.floor(NOW / 1000) - days * 86_400,
  layers: [BASES[base]!, ...own].map(([cmd, mb]) => ({ diff: `sha256:${hash(cmd)}`, size: Math.round(mb * 1_000_000), created_by: cmd })),
}));

/** The mock's volumes: MB, age in days, top-level entries (a directory's
 * name ends in `/`). */
const VOLUMES: [string, number, number, string[]][] = [
  ["pgdata", 1240, 21, ["PG_VERSION", "base/", "global/", "pg_wal/", "pg_stat/", "postgresql.conf", "postmaster.pid"]],
  ["redis-data", 46, 21, ["appendonlydir/", "dump.rdb"]],
  ["uploads", 312, 21, ["2026/09/", "2026/10/", "thumbs/", ".keep"]],
  ["cargo-cache", 2150, 60, ["registry/index/", "registry/cache/", "registry/src/", "git/db/"]],
  ["old-pgdata", 840, 270, ["PG_VERSION", "base/", "global/", "pg_wal/"]],
];
let storedVolumes: StoredVolume[] = VOLUMES.map(([name, mb, days, files]) => ({
  name,
  created: new Date(NOW - days * 86_400_000).toISOString(),
  size: mb * 1_000_000,
  complete: true,
  files: files.map((file) => (file.endsWith("/") ? { name: file.slice(0, -1), kind: "directory", size: 4096 } : { name: file, kind: "file", size: 512 })),
  more: 0,
  mountpoint: `/var/lib/shipyard/volumes/${name}/_data`,
}));

/** Shipyard's own view of network `name`, its veths as the mock names them
 * (the container's name and the network's short one, hashed). */
function networkView(name: string): NetworkView | null {
  const network = NETWORKS.find((n) => n.name === name);
  if (!network) return null;
  const on = SPECS.filter((spec) => ATTACH[spec.name]?.nets[name]);
  return {
    name,
    bridge: BRIDGES[name] ?? "",
    gateway: network.gateway ?? "",
    internal: network.internal,
    dns_running: name !== "bridge",
    endpoints: on.map((spec) => ({
      container: idOf(spec),
      address: ATTACH[spec.name]?.nets[name] ?? "",
      host_veth: `veth${hash(spec.name + network.short).slice(0, 5)}`,
    })),
  };
}

/** Container `id`'s spec changed by `to`, as the mock's `act` does. */
function change(id: string, to: Partial<Spec>) {
  const spec = SPECS.find((s) => idOf(s) === id);
  if (spec) Object.assign(spec, to);
}

/** A backend that serves the mock's data, its VM up. */
export const fixtureBackend: Backend = {
  status: async () => ({
    platform: "macos",
    vm: { state: "running", phase: "up", serving: true },
    startVmOnOpen: true,
    shipyard: { path: "/usr/local/bin/shipyard" },
    settingsSaid: null,
  }),
  startVm: async () => {},
  snapshot: async () => snapshot(),
  links: async () => snapshot().links,
  usage: async () => usage(),
  startContainer: async (id) => change(id, { state: "running", exit: null, startedMinutesAgo: 0, status: "Up Less than a second" }),
  stopContainer: async (id) => change(id, { state: "exited", exit: 0, startedMinutesAgo: null, status: "Exited (0) Less than a second ago" }),
  restartContainer: async (id) => change(id, { state: "running", exit: null, startedMinutesAgo: 0, status: "Up Less than a second" }),
  pauseContainer: async (id) => change(id, { state: "paused", status: "Up Less than a second (Paused)" }),
  unpauseContainer: async (id) => change(id, { state: "running", status: "Up Less than a second" }),
  removeContainer: async (id) => {
    const at = SPECS.findIndex((spec) => idOf(spec) === id);
    if (at >= 0) SPECS.splice(at, 1);
  },
  // A standalone container, as the mock's run makes one.
  runImage: async (spec) => {
    const serial = String(400000 + SPECS.length);
    const image = spec.image.includes(":") ? spec.image : `${spec.image}:latest`;
    const name = spec.name || (spec.image.split("/").pop()?.split(":")[0] ?? "box");
    const port = spec.port ? spec.port.split(":").map(Number) : null;
    const added: Spec = {
      serial,
      name,
      project: null,
      image,
      state: "running",
      exit: null,
      memMax: spec.memoryMib,
      mem: 4,
      cpu: 1,
      ports: port ? [[port[0] ?? 0, port[1] ?? port[0] ?? 0]] : [],
      startedMinutesAgo: 0,
      status: "Up Less than a second",
    };
    SPECS.push(added);
    return idOf(added);
  },
  onRunLine: async () => () => {},
  images: async () => storedImages,
  volumes: async () => ({ version: 1, volumes: storedVolumes, warnings: [] }),
  removeVolume: async (name) => {
    storedVolumes = storedVolumes.filter((volume) => volume.name !== name);
  },
  networkView: async (name) => networkView(name),
  removeImage: async (reference) => {
    storedImages = storedImages.filter((image) => !image.tags.includes(reference) && image.id !== reference);
  },
  logsFollow: async (id, _follow, _selection, onbatch) => {
    const spec = SPECS.find((s) => idOf(s) === id);
    onbatch({ lines: LOGS[spec?.name ?? ""] ?? [], ended: null });
  },
  logsStop: async () => {},
  // A shell that answers what's typed, behind the mock's prompt; `link`
  // answers as a hostile container could: a link, a clipboard write and a
  // title, in escape sequences (the CSP test's).
  shellOpen: async (_id, _cols, _rows, onoutput) => {
    shellOutput = onoutput;
    typed = "";
    onoutput(new TextEncoder().encode("/ # "));
    return 1;
  },
  shellInput: async (_session, data) => {
    let echoed = data;
    if (data === "\r") {
      echoed =
        typed === "link"
          ? "\r\n\x1b]8;;https://example.test/\x07open me\x1b]8;;\x07 \x1b]52;c;cHduZWQ=\x07\x1b]0;pwned\x07\r\n/ # "
          : "\r\n/ # ";
      typed = "";
    } else typed += data;
    shellOutput?.(new TextEncoder().encode(echoed));
  },
  shellResize: async () => {},
  shellAck: async () => {},
  shellClose: async () => {
    shellOutput = null;
  },

  isolation: async (id) => isolationOf(id),
  // The mock's worker was killed out of memory (its 137).
  inspect: async (id) => ({ State: { OOMKilled: SPECS.find((s) => idOf(s) === id)?.exit === 137 } }),
  // The mock's numbers at once, as its series are (Math.random at 0.5).
  watchStats: async (id) => {
    const spec = SPECS.find((s) => idOf(s) === id);
    if (!spec) return;
    for (let i = 0; i < SAMPLES; i++) {
      statsHandler?.({ id, cpu: spec.cpu, mem: spec.mem * MIB, limit: spec.memMax * MIB });
    }
  },
  unwatchStats: async () => {},
  onStatsEnded: async () => () => {},
  onStats: async (handler) => {
    statsHandler = handler;
    return () => {};
  },
  follow: async () => {},
  release: async () => {},
  setStartVmOnOpen: async () => {},
  chooseShipyard: async () => {},
  onEvent: async () => () => {},
  onVmLine: async () => () => {},
  onLost: async () => () => {},
};
